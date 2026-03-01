# Design Document: Test Temporary Directory Cleanup

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Unit tests create ~359 temporary directories per `cargo test` run with no cleanup, accumulating ~14,000 orphaned inodes in `/tmp/`. This document introduces a `TestDir` RAII guard that auto-removes directories on drop, applied via a single helper function used by all tests.

## Problem Statement

### Background

The test suite (1687 tests, 19 source files) uses `std::env::temp_dir().join(format!("loopr-{prefix}-{id}"))` to create unique temp directories for test isolation. Each test that needs a `Stores` instance, an `AgentLogger`, or a `TaskStore` creates a fresh directory. The `generate_id()` call ensures uniqueness but nothing ensures cleanup.

### Problem

359 tests across 19 files create temp directories. Only 3-9 (<3%) clean up after themselves. Each directory contains a `.taskstore/` subdirectory with 9 JSONL files, a SQLite database, and optionally a `.git/` tree from `git init`. At ~40 inodes per test directory, a single `cargo test` run orphans ~14,360 inodes. Multiple runs compound — 2,254 stale directories were found in `/tmp/` during today's session.

This causes:
- **Inode exhaustion** on machines with many test runs, leading to "No space left on device" errors even with free disk space
- **Slow /tmp enumeration** — 2000+ directories degrade `ls`, `find`, and tmpwatch performance
- **CI flakiness** — shared CI runners accumulate dirs across jobs; `/tmp` is rarely cleaned between runs

### Root Cause

No automated cleanup mechanism. The project does not use the `tempfile` crate. Tests call `std::fs::create_dir_all()` directly and rely on the OS to eventually clear `/tmp/` (which many systems never do for non-reboot `/tmp`).

The pattern is consistent across all 19 files:
```rust
let dir = std::env::temp_dir().join(format!("loopr-{prefix}-{}", crate::id::generate_id()));
std::fs::create_dir_all(&dir).unwrap();
```

No corresponding `std::fs::remove_dir_all(&dir)` exists at the end of the test.

### Goals

- Zero orphaned test directories after `cargo test` completes
- Minimal change footprint — one new helper, mechanical replacement in existing tests
- No test behavior changes — same directory paths, same isolation
- Works with `#[test]` and `#[tokio::test]`

### Non-Goals

- Changing test logic or assertions
- Consolidating or reducing the number of tests
- Replacing `generate_id()` with sequential counters
- Cleaning up non-test temp directories (daemon worktrees, etc.)

## Proposed Solution

### Overview

Introduce a `TestDir` struct that wraps a `PathBuf` and implements `Drop` to call `remove_dir_all`. `TestDir::new(prefix)` replaces the inline `temp_dir().join(format!(...))` + `create_dir_all` pattern. Apply it to all 359 test sites via mechanical find-and-replace.

### Implementation

#### Phase 1: Add `TestDir` to test utilities

Create the guard in a `#[cfg(test)]` module accessible to all test code:

**File:** `src/test_util.rs` (new file, `#[cfg(test)]` gated)

```rust
use std::ops::Deref;
use std::path::{Path, PathBuf};

/// RAII guard for test temporary directories.
/// Creates the directory on construction, removes it on drop.
/// Implements `Deref<Target=Path>` so `&dir` auto-coerces to `&Path`,
/// and `dir.join(...)`, `dir.display()` etc. work directly.
pub struct TestDir(PathBuf);

impl TestDir {
    /// Create a new temp directory with the given prefix.
    /// Directory is created immediately; removed when this guard is dropped.
    pub fn new(prefix: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("{}-{}", prefix, crate::id::generate_id()));
        std::fs::create_dir_all(&dir).expect("failed to create test dir");
        Self(dir)
    }
}

impl Deref for TestDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
```

Wire into `src/lib.rs`:
```rust
#[cfg(test)]
pub mod test_util;
```

#### Phase 2: Migrate all 359 test sites

The transformation at each call site:

```rust
// Before:
let dir = std::env::temp_dir().join(format!("loopr-coord-xxx-{}", crate::id::generate_id()));
std::fs::create_dir_all(&dir).unwrap();
let stores = test_stores(&dir);

// After:
let dir = TestDir::new("loopr-coord-xxx");
let stores = test_stores(&dir);
```

Helper signatures stay the same (`&Path`). Thanks to `Deref<Target=Path>`, `&dir` auto-coerces to `&Path`, `dir.join(...)` works, and no `.path()` calls are needed. The `dir` binding ensures the guard lives for the test's full scope. On test exit (pass or panic), `Drop` fires and removes the directory.

### Migration Pattern

The transformation is mechanical. For each test:

1. Find: `let dir = std::env::temp_dir().join(format!("loopr-{PREFIX}-{}", crate::id::generate_id()));`
2. Find: `std::fs::create_dir_all(&dir).unwrap();`
3. Replace both with: `let dir = TestDir::new("loopr-{PREFIX}");`
4. No further changes needed — `Deref<Target=Path>` makes `&dir` coerce to `&Path` and `dir.join(...)` work as before

### File-by-file Scope

| File | Test sites | Priority |
|------|-----------|----------|
| `src/agents/coordinator.rs` | 76 | High — largest contributor |
| `src/agents/executor.rs` | 69 | High |
| `src/agents/generation.rs` | 42 | Medium |
| `src/agents/implementer.rs` | 39 | Medium |
| `src/agents/integrator.rs` | 31 | Medium |
| `src/agents/researcher.rs` | 28 | Medium |
| `src/agents/context.rs` | 17 | Low |
| `src/agents/reviewer.rs` | 12 | Low |
| `src/integration_tests.rs` | 8 | Low |
| 10 other files | 1-5 each | Low |

### Implementation Plan

| Phase | What | Files |
|-------|------|-------|
| 1 | Create `TestDir` struct + wire into `lib.rs` | `src/test_util.rs` (new), `src/lib.rs` |
| 2 | Migrate all 359 test sites across 19 files | All files listed in scope table above |

Phase 2 is mechanical find-and-replace, one file at a time. Each file can be committed independently. Run `otto ci` after each file to catch regressions immediately.

**Edge case:** A few helpers (`daemon/context.rs:test_config`, `daemon/mod.rs`, `agents/bridge.rs`) use `dir` as an owned `PathBuf` (e.g., `repo_path: dir` or `repo_path: dir.clone()`). After migration, these become `repo_path: dir.to_path_buf()` since `TestDir` derefs to `&Path`, not `PathBuf`.

## Alternatives Considered

### Alternative 1: `tempfile` crate (`TempDir`)

- **Description:** Add the `tempfile` crate as a dev-dependency and use `TempDir::new()` which auto-cleans on drop.
- **Pros:** Battle-tested, handles edge cases (permissions, symlinks). Well-known in the Rust ecosystem.
- **Cons:** External dependency for a trivial wrapper. `TempDir::new()` uses a random name — loses the descriptive `loopr-coord-xxx` prefix that makes debugging easier. `TempDir::with_prefix()` exists but still appends random suffix, changing the naming convention.
- **Why not chosen:** The project convention is zero external test dependencies and descriptive directory names. A 15-line `TestDir` achieves the same RAII cleanup without adding a crate or losing naming control.

### Alternative 2: Explicit `remove_dir_all` in each test

- **Description:** Add `std::fs::remove_dir_all(&dir).ok();` at the end of each test function.
- **Pros:** No new types. Direct, obvious.
- **Cons:** 359 tests to modify. Doesn't run on panic (test failures skip cleanup). Easy to forget in new tests.
- **Why not chosen:** RAII (Drop) is strictly better — runs on both success and panic, and a single constructor prevents forgetting cleanup.

### Alternative 3: Global test fixture with `ctor`/`dtor`

- **Description:** Use a `#[dtor]` to glob-remove `/tmp/loopr-*` after the test binary exits.
- **Pros:** One cleanup point. No per-test changes.
- **Cons:** `ctor` crate dependency. Destructors run after all tests — parallel tests that outlive the main thread may race. Removes ALL loopr temp dirs, including those from a running daemon.
- **Why not chosen:** Too coarse. Interferes with daemon temp dirs and parallel test runs.

### Alternative 4: Do nothing, rely on tmpwatch/systemd-tmpfiles

- **Description:** Let the OS clean `/tmp/` on its own schedule (systemd-tmpfiles cleans files older than 10 days by default).
- **Pros:** Zero code changes.
- **Cons:** Doesn't solve inode accumulation within a single day of development. CI runners may not have tmpwatch. 14,000 inodes per run × 10 runs/day = 140,000 orphaned inodes before cleanup.
- **Why not chosen:** The accumulation rate exceeds the cleanup rate on active development machines.

## Technical Considerations

### Dependencies

None. The `TestDir` struct uses only `std::path`, `std::fs`, and the existing `crate::id::generate_id()`.

### Performance

- `remove_dir_all` in `Drop` adds ~1ms per test (dominated by SQLite file deletion). With 359 tests, total overhead: ~360ms across the full suite — negligible against the ~6s test runtime.
- Reduces `/tmp` enumeration overhead for subsequent runs.

### Testing Strategy

- `TestDir` itself gets 2 tests: creation + `Deref`, and cleanup on drop (verify dir doesn't exist after guard goes out of scope)
- Each migrated file is validated by `otto ci` after migration — same test count, same pass rate
- Verify `/tmp/loopr-*` count is 0 after `cargo test` completes

### Rollout Plan

One commit per phase. Each commit is independently valid (partial migration is fine — un-migrated tests still work, they just don't clean up). No behavioral changes to any test.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `remove_dir_all` fails silently on Windows/macOS locked files | Low | Low | `let _ =` ignores errors. Test still passes. Dir may persist — same as today. |
| `TestDir` dropped before test body finishes (moved/consumed) | Low | Med | `TestDir` is not `Copy`/`Clone`. Borrow checker prevents premature drop. Tests hold the binding for their full scope. |
| New tests forget to use `TestDir` | Med | Low | Code review. Could add a clippy lint or grep-based CI check for raw `temp_dir()` in test code. |
| Parallel tests racing on same-prefix directories | None | None | `generate_id()` ensures unique paths. Prefix is for human readability only. |

## Open Questions

- [ ] Should we add a CI gate that fails if `/tmp/loopr-*` dirs exist after test completion?

## References

- `src/agents/coordinator.rs` — 76 test sites (largest)
- `src/agents/executor.rs` — 69 test sites
- `src/agents/generation.rs` — 42 test sites
- `crate::id::generate_id()` — ULID generator used for unique dir names
- `tempfile` crate docs: https://docs.rs/tempfile/latest/tempfile/
