# Design Document: Test Block Extraction

**Author:** Scott A. Idler
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 3/3

## Summary

Six source files exceed 1,500 lines primarily because their `#[cfg(test)] mod tests { ... }` blocks are co-located with production code. Extracting each test block to a parallel `foo/tests.rs` file using the modern Rust inline-module pattern (`foo.rs` + `foo/`) drops all six files below 800 lines with zero behavioral change.

## Problem Statement

### Background

Loopr has 9 source files over 1,500 lines targeted for decomposition. For 6 of them, the majority of the line count is test code, not production code. The test blocks inflate context window cost and obscure production logic without adding any coupling value - they are already isolated behind `#[cfg(test)]`.

### Problem

Test code co-located in large source files:
- Inflates the file size seen by rust-analyzer and LLM context windows
- Makes it harder to navigate to production code
- Creates merge conflicts between test changes and production changes on the same file

### Goals

- Drop all 6 files below 1,500 lines (target: under 800)
- Zero behavioral changes
- Preserve private item access in tests via `super::`
- Each extraction is a standalone passing commit
- Use modern Rust module pattern: `foo.rs` + `foo/tests.rs` (no `mod.rs`)

### Non-Goals

- Reorganizing or refactoring the test code itself
- Subdividing tests by topic within a file
- Addressing the remaining 3 oversized files (`coordinator.rs`, `tui/run.rs`, `agents/mod.rs`)
- Any production code changes

## Proposed Solution

### Overview

For each file, use Rust's inline module resolution: a file `foo.rs` can declare `mod bar;` which resolves to `foo/bar.rs`. The `foo.rs` file and `foo/` directory coexist. This means `foo.rs` can declare `#[cfg(test)] mod tests;` and the compiler resolves it to `foo/tests.rs` - no `mod.rs` required, private access preserved.

### Mechanical Steps (per file)

Given `src/agents/foo.rs` containing:
```rust
// ... production code ...

#[cfg(test)]
mod tests {
    use super::*;
    // ... test functions ...
}
```

1. `mkdir src/agents/foo/`
2. Create `src/agents/foo/tests.rs` with the block's inner content (everything between the outer braces, not including the `mod tests {` line itself)
3. In `src/agents/foo.rs`, replace the entire `#[cfg(test)] mod tests { ... }` block with the single line `#[cfg(test)] mod tests;`
4. `cargo test` - output must be identical to pre-extraction

### Files and Impact

| Source file | Current lines | Tests start | After extraction | Tests file |
|-------------|--------------|-------------|-----------------|------------|
| `src/agents/implementer.rs` | 2,078 | 652 | ~651 | `src/agents/implementer/tests.rs` |
| `src/daemon/handlers/bundle.rs` | 1,564 | 536 | ~535 | `src/daemon/handlers/bundle/tests.rs` |
| `src/cli/dispatch.rs` | 1,591 | 575 | ~574 | `src/cli/dispatch/tests.rs` |
| `src/agents/context.rs` | 1,854 | 776 | ~775 | `src/agents/context/tests.rs` |
| `src/agents/generation.rs` | 2,293 | 936 | ~935 | `src/agents/generation/tests.rs` |
| `src/agents/integrator.rs` | 2,220 | 1,044 | ~1,043 | `src/agents/integrator/tests.rs` |

### Commit Order

Ordered smallest resulting file first (least residual complexity):

1. `src/daemon/handlers/bundle.rs` (~535 lines remaining)
2. `src/cli/dispatch.rs` (~574 lines remaining)
3. `src/agents/context.rs` (~775 lines remaining)
4. `src/agents/implementer.rs` (~651 lines remaining)
5. `src/agents/generation.rs` (~935 lines remaining)
6. `src/agents/integrator.rs` (~1,043 lines remaining)

Each commit: `refactor(tests): extract test block from <module> into <module>/tests.rs`

## Alternatives Considered

### Alternative 1: Top-level `tests/` directory (Python-style)

- **Description:** Mirror `src/` structure under a root-level `tests/` directory.
- **Pros:** Clean separation, idiomatic in Python.
- **Cons:** Root `tests/` in Cargo is for integration tests compiled as a separate crate - no access to private items. Would require auditing and `pub(crate)`-ing all tested private helpers.
- **Why not chosen:** Too much churn for a structural-only refactor.

### Alternative 2: Keep tests in-file, extract other code instead

- **Description:** Leave tests where they are, move production code out first.
- **Pros:** Avoids the module-dir churn.
- **Cons:** Harder - production code has more dependencies. Tests are the easiest isolated block to move.
- **Why not chosen:** Misses the easy win. Test extraction is lower risk.

### Alternative 3: `foo/mod.rs` pattern

- **Description:** Rename `foo.rs` to `foo/mod.rs` to enable subdirectories.
- **Pros:** Works.
- **Cons:** `mod.rs` is the old Rust idiom. Conflicts with in-progress effort to modernize module structure.
- **Why not chosen:** The `foo.rs` + `foo/` inline pattern supersedes this.

## Technical Considerations

### Dependencies

No external dependencies. Pure file reorganization within the existing crate.

### Performance

Zero runtime impact. `#[cfg(test)]` blocks are compiled out in release builds regardless of location.

### Security

No implications.

### Testing Strategy

`cargo test` after each extraction. Tests must pass identically before and after - same test count, same names, same results.

### Rollout Plan

Sequential commits on the current branch, one file per commit. No feature flags. No coordination required.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Test uses private helper not accessible via `super::` | Low | Low | `super::` reaches all private items in parent module - this is equivalent to the original `mod tests` inline |
| Compiler can't resolve `foo/tests.rs` path | Low | Low | Rust 2018+ inline module resolution is stable; verify `edition = "2021"` in Cargo.toml |
| Merge conflict with concurrent work on same file | Low | Low | Each extraction is a single clean commit; coordinate with active branches |

## Open Questions

None.

## References

- [2026-04-01-large-file-decomposition.md](2026-04-01-large-file-decomposition.md) - Parent decomposition effort
- [decomposition.yml](decomposition.yml) - Line-range mapping for all 9 files
