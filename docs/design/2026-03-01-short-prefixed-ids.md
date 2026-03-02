# Design Document: Short Prefixed IDs

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Implemented (5-char)
**Review Passes Completed:** 5/5

## Summary

Replace 26-character ULID identifiers with human-readable `{2-letter prefix}-{5-char base36}` IDs (8 chars total) across all 14 Record-implementing entities. This makes JSONL logs, TUI displays, and debugging dramatically easier without sacrificing uniqueness within a single run.

## Problem Statement

### Background

Loopr v3 uses ULIDs for all entity IDs. ULIDs are time-ordered and globally unique, but their 26-character Crockford Base32 encoding (e.g. `01KJP1DN7Z870MZ2PSJS4XZB2M`) is unreadable in logs, the TUI, and during debugging. When scanning a `works.jsonl` file or reading coordinator logs, these IDs are opaque noise.

### Problem

Long, undifferentiated IDs make it impossible to visually distinguish entity types or trace relationships in logs. A Work ID looks identical to a Phase ID looks identical to a Bundle ID. This hurts debuggability and developer experience.

### Goals

- Replace ULID IDs with short, prefixed, human-readable IDs
- Format: `{2-letter prefix}-{5-char random base36}` (e.g. `wk-k7m2p`)
- Prefix immediately identifies entity type
- Sufficient uniqueness for single-run contexts (~60M combinations per prefix)
- Minimal code churn — single function signature change propagated to 14 call sites

### Non-Goals

- Cross-run global uniqueness (not needed; each run has its own JSONL store)
- Time-ordering of IDs (not used anywhere; entities have `created_at` timestamps)
- Migration of existing data (runs are ephemeral)
- Changing the `Record` trait in taskstore (it accepts `&str`, agnostic to format)

## Proposed Solution

### Overview

Change `generate_id()` to accept a `&str` prefix, produce 5 random lowercase base36 characters, and return `"{prefix}-{random}"`. Update all 14 call sites to pass their entity prefix.

### Prefix Assignments

| Entity | Prefix | Example ID |
|---|---|---|
| Plan | `pl` | `pl-k7m2p` |
| Spec | `sp` | `sp-a3f9x` |
| Phase | `ph` | `ph-82kb4` |
| Work | `wk` | `wk-n4p7q` |
| Bundle | `bd` | `bd-x9r2s` |
| Tick | `tk` | `tk-j5t8v` |
| Lock | `lk` | `lk-m1w3y` |
| CoordinatorGoal | `cg` | `cg-b6z4c` |
| CoordinatorState | `cs` | `cs-d8f5g` |
| Proposal | `pr` | `pr-h2k7n` |
| Decision | `dc` | `dc-p3q9r` |
| Learning | `ln` | `ln-s5u1w` |
| ValidationReport | `vr` | `vr-v7x4z` |
| AgentSession | `ag` | `ag-t6y2b` |

### Data Model

No structural changes to any domain struct. The `id: String` field stays the same — only its generated content changes from 26-char ULID to 8-char prefixed ID.

### API Design

```rust
// src/id.rs — BEFORE
pub fn generate_id() -> String {
    Ulid::new().to_string()
}

// src/id.rs — AFTER
pub fn generate_id(prefix: &str) -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let code: String = (0..5)
        .map(|_| {
            let idx = rng.random_range(0..36u8);
            if idx < 10 { (b'0' + idx) as char }
            else { (b'a' + idx - 10) as char }
        })
        .collect();
    format!("{prefix}-{code}")
}
```

The `ulid` crate dependency is removed from `Cargo.toml`. The `rand` crate must be added as a direct dependency (`cargo add rand`) — it currently exists only as a transitive dependency and will not survive the `ulid` removal.

### Implementation Plan

**Phase 1: Update `id.rs`** (single file)
- Change `generate_id()` signature to `generate_id(prefix: &str) -> String`
- Replace ULID generation with `rand`-based 5-char base36
- Update tests: length check (26 → 8), format validation, uniqueness, remove lexicographic order test
- `cargo add rand` (direct dependency — currently only transitive)
- `cargo remove ulid`

**Phase 2: Update all 14 call sites** (mechanical)
- `src/domain/plan.rs` → `id::generate_id("pl")`
- `src/domain/spec.rs` → `id::generate_id("sp")`
- `src/domain/phase.rs` → `id::generate_id("ph")`
- `src/domain/work.rs` → `id::generate_id("wk")`
- `src/domain/bundle.rs` → `id::generate_id("bd")`
- `src/domain/tick.rs` → `id::generate_id("tk")`
- `src/domain/lock.rs` → `id::generate_id("lk")`
- `src/domain/coordinator_goal.rs` → `id::generate_id("cg")`
- `src/domain/coordinator_state.rs` → `id::generate_id("cs")`
- `src/domain/proposal.rs` → `id::generate_id("pr")`
- `src/domain/decision.rs` → `id::generate_id("dc")`
- `src/domain/learning.rs` → `id::generate_id("ln")`
- `src/domain/validation.rs` → `id::generate_id("vr")`
- `src/agents/mod.rs` (AgentSession) → `id::generate_id("ag")`

**Phase 3: Update non-domain call sites** (mechanical)
- `src/ipc/client.rs` — temp socket names (keep as-is or switch; cosmetic only)
- `src/test_util.rs` — temp dir names (keep as-is or switch; cosmetic only)
- `src/tui/run.rs` — test socket names (keep as-is or switch; cosmetic only)
- `src/validator/client.rs` — test env var names (keep as-is or switch; cosmetic only)
- `src/validator/mod.rs` — test env var names (keep as-is or switch; cosmetic only)
- `src/agents/coordinator.rs:3592` — test fixture (use `id::generate_id("ln")`)

Non-domain call sites just need a unique string for temp files/sockets. Use `"xx"` as a generic prefix for these — they are never stored as Records and the prefix is irrelevant.

**Phase 4: Validate**
- `otto ci` — full pipeline

## Alternatives Considered

### Alternative 1: Truncated ULID (keep first 8 chars)
- **Description:** Keep ULID but truncate to 8 characters, add prefix
- **Pros:** Retains partial time-ordering
- **Cons:** Crockford Base32 uses uppercase + digits — inconsistent aesthetic. ULID's time component is in the first 10 chars so 8 isn't enough for meaningful ordering anyway.
- **Why not chosen:** No benefit over random base36 at this length, and the mixed case is ugly.

### Alternative 2: Sequential counter per prefix
- **Description:** `wk-00001`, `wk-00002`, etc. using an atomic counter
- **Pros:** Deterministic ordering, very readable
- **Cons:** Requires shared mutable state (atomic counter per entity type). Counters reset across process restarts within a run. Would need persistence or coordination.
- **Why not chosen:** Added complexity for minimal benefit. Random is simpler and `created_at` already provides ordering.

### Alternative 3: Keep ULID, just add prefix
- **Description:** `wk-01KJP1DN7Z870MZ2PSJS4XZB2M` (28 chars)
- **Pros:** Zero collision risk
- **Cons:** Still unreadable. The whole point is shorter IDs.
- **Why not chosen:** Doesn't solve the problem.

## Technical Considerations

### Dependencies

- **Add:** `rand` (direct dependency — currently only transitive via `ulid`)
- **Remove:** `ulid` crate

### Collision Risk

Current implementation uses 5 base36 characters:

| Chars | Combinations (per prefix) | 50% collision threshold |
|-------|--------------------------|------------------------|
| 5 (current) | 36^5 = 60,466,176 | ~11,000 entities |
| 6 (future) | 36^6 = 2,176,782,336 | ~55,000 entities |

A single Loopr run creates at most hundreds of entities per type. Risk is negligible at 5 chars. If entity counts ever grow substantially, bumping to 6 chars is a one-line change (`0..5` → `0..6` in `generate_id`).

### Performance

Random generation via `rand::rng()` is faster than ULID generation. No performance concern.

### Testing Strategy

- Update `id.rs` unit tests: format validation (`{2 letters}-{5 alphanum}`), uniqueness (100 IDs), prefix passthrough
- All existing domain tests continue to work unchanged (they don't assert on ID format beyond it being a non-empty string)
- `otto ci` validates the full pipeline

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| ID collision within a run | Negligible | Medium | 60M combinations per prefix at 5 chars; bump to 6 chars (~2.2B) if needed |
| Existing test assertions on ID length/format | Low | Low | Only `id.rs` tests check format; update them |
| Non-domain `generate_id` callers break | Certain | Low | Mechanical: add a prefix arg to each call site |

## Open Questions

None — scope is well-defined and self-contained.

## References

- Current ID module: `src/id.rs`
- TaskStore `Record` trait: requires `fn id(&self) -> &str` (format-agnostic)
- ULID spec: https://github.com/ulid/spec (being removed)
