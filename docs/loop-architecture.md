# Design Document: Loopr Loop Architecture

**Author:** Scott Idler, Claude
**Date:** 2026-01-25
**Status:** Implementation Spec
**Review Passes Completed:** 5/5

## Summary

Loopr orchestrates hierarchical Ralph Wiggum loops for autonomous software development. Four loop types (Plan, Spec, Phase, Ralph) form a hierarchy where each level produces artifacts that spawn child loops. Every loop follows the Ralph Wiggum pattern: fresh context per iteration, prompt updated with failure feedback, iterate until validation passes.

## Problem Statement

### Background

The Ralph Wiggum technique (Geoffrey Huntley) enables autonomous LLM coding by forcing the model to confront its own failures:

```bash
while :; do cat PROMPT.md | claude ; done
```

Key insight: fresh context each iteration prevents "context rot" - the LLM doesn't accumulate confusion from failed attempts.

### Problem

How do we orchestrate multiple Ralph loops in a hierarchy where:
- Each level produces artifacts that define work for the next level
- Any level can fail validation and re-iterate
- Re-iteration at outer levels invalidates work done by inner levels (the "onion problem")
- All state must be preserved for debugging and resumption

### Goals

- Define the four-level loop hierarchy (Plan → Spec → Phase → Ralph)
- Specify how artifacts connect parent loops to child loops (connective tissue)
- Design storage layout for loops, iterations, and artifacts
- Solve the invalidation cascade problem when outer loops re-iterate
- Enable fast queries over loop state via TaskStore

### Non-Goals

- Prompt engineering (what goes in each loop's prompt)
- Validation logic (how each loop type validates success)
- TUI implementation details
- LLM provider abstraction

## Proposed Solution

### Overview: Two Core Concepts

1. **The Hierarchy**: Plan → Spec → Phase → Ralph, with artifacts as connective tissue
2. **The Ralph Pattern**: Each level is itself a Ralph Wiggum loop with fresh context per iteration

These create an **onion structure** - loops within loops, where outer layers produce artifacts that spawn inner layers.

### Architecture

#### The Hierarchy

```
Conversation (user interaction)
└── spawns 1 PlanLoop
    └── produces 1+ plan.md artifacts       ← CONNECTIVE TISSUE
        └── each plan.md spawns 1 SpecLoop
            └── produces 1+ spec.md artifacts   ← CONNECTIVE TISSUE
                └── each spec.md spawns 1 PhaseLoop
                    └── produces 3-7 phase.md artifacts ← CONNECTIVE TISSUE
                        └── each phase.md spawns 1 RalphLoop
                            └── produces code/docs/examples
```

#### Artifacts as Connective Tissue

| Parent Loop | Produces Artifact | Spawns Child |
|-------------|-------------------|--------------|
| PlanLoop | `plan.md` | SpecLoop |
| SpecLoop | `spec.md` | PhaseLoop |
| PhaseLoop | `phase.md` | RalphLoop |
| RalphLoop | code files | (none - leaf node) |

**A child loop is always spawned *from* a specific artifact produced by its parent.** The artifact is the contract between layers - it defines what the child must accomplish.

**Implementation:** TaskStore relationships (`parent_loop` + `triggered_by` fields) are the connective tissue - they link each child to the specific artifact that spawned it.

#### Every Loop is a Ralph Wiggum Loop

**All four loop types (Plan, Spec, Phase, Ralph) follow this same pattern:**

1. **Fresh context window** each iteration (no LLM memory - prevents context rot)
2. **Prompt** = base prompt + feedback from previous iteration (nothing extra on iteration 1)
3. **Validation** determines if iteration succeeded or needs retry
4. **Iterate** until validation passes or max iterations reached
5. **Produce artifacts** that may spawn child loops

#### How Fresh Context + Learning Works

Since each iteration starts with a fresh LLM context, the **prompt itself must be updated** to include:

- What was tried last time
- Why validation failed
- Error messages
- Progress made so far

Example prompt evolution:

```
Iteration 1 prompt:
  "Create a plan for feature X..."

Iteration 2 prompt:
  "Create a plan for feature X...

   PREVIOUS ATTEMPT FAILED:
   - Missing security section
   - No migration strategy

   Please address these issues."

Iteration 3 prompt:
  "Create a plan for feature X...

   PREVIOUS ATTEMPTS:
   - Iteration 1: Missing security section, no migration strategy
   - Iteration 2: Security added but migration still incomplete

   Please complete the migration strategy."
```

**Each iteration's prompt is different and must be preserved.**

#### Loop Lifecycle

```
pending → running → [iterating] → complete
                 ↘             ↗
                   failed/paused
                        ↓
                   invalidated (if parent re-iterates)
```

State transitions:
- **pending → running**: Loop execution starts
- **running (iterating)**: Each iteration runs validation; fail → increment iteration, update prompt; pass → complete
- **running → complete**: Validation passed, artifacts produced
- **running → failed**: Max iterations reached without validation passing
- **running → paused**: Manual pause or resource constraint
- **any → invalidated**: Parent loop re-iterated, this loop's triggering artifact is stale

### Data Model

#### Storage Layout

```
~/.loopr/<project-hash>/
├── .taskstore/
│   ├── loops.jsonl        # All loop records
│   └── taskstore.db       # SQLite cache (derived, regenerable)
├── loops/
│   └── <loop-id>/
│       ├── iterations/
│       │   └── 001/
│       │       ├── prompt.md           # What we sent to LLM
│       │       ├── conversation.jsonl  # LLM responses + tool calls
│       │       ├── validation.log      # Why it failed (if it did)
│       │       └── artifacts/          # What this iteration produced
│       ├── stdout.log                  # Streamable aggregate
│       ├── stderr.log                  # Streamable aggregate
│       └── current -> iterations/NNN/  # Symlink to latest iteration
└── archive/                            # Invalidated loops moved here
    └── <loop-id>/
```

#### Loop Record Schema (TaskStore)

Each loop stored in `loops.jsonl`:

```json
{
  "id": "1737802800",
  "type": "spec",
  "status": "running",
  "parent_loop": "1737800000",
  "triggered_by": "iterations/002/artifacts/plan-auth.md",
  "conversation_id": "conv-abc123",
  "iteration": 3,
  "max_iterations": 50,
  "created_at": 1737802800000,
  "updated_at": 1737803700000
}
```

Key fields:
- **id**: Timestamp-based (e.g., `1737802800`)
- **type**: `plan` | `spec` | `phase` | `ralph`
- **status**: `pending` | `running` | `paused` | `complete` | `failed` | `invalidated`
- **parent_loop**: ID of parent loop (null for PlanLoop)
- **triggered_by**: Path to artifact that spawned this loop (includes parent's iteration number)
- **conversation_id**: Reference to originating TUI conversation (inherited from PlanLoop down to all children)
- **iteration**: Current iteration count

**Note:** Parent's iteration is embedded in `triggered_by` path. To detect stale children: parse iteration from path, compare to parent's current `iteration` field.

### The Onion Problem: Invalidation Strategy

When an outer layer re-iterates, inner layers become stale.

#### Example: Deep Cascade

```
PlanLoop iter 1 → plan-v1.md
  └── SpecLoop iter 1-3 → spec-v1.md (validated after 3 tries)
        └── PhaseLoop iter 1-2 → phase-v1.md, phase-v2.md, phase-v3.md
              └── RalphLoop-A → wrote 200 lines of auth code
              └── RalphLoop-B → wrote 300 lines of API code
              └── RalphLoop-C → wrote 150 lines of tests

NOW: SpecLoop validation fails on higher-level review, needs iter 4
     SpecLoop iter 4 → spec-v2.md (different from spec-v1.md)

QUESTION: What happens to PhaseLoop's work? RalphLoop's 650 lines of code?
```

#### Solution: Archive + Git Branches

1. **Archive children** - move invalidated loop directories to `~/.loopr/<project>/archive/<loop-id>/`, update status to `invalidated` in TaskStore (record stays, files move)
2. **Git branches** - each iteration's code lives on `loop-<id>-iter-<N>` branch; re-iteration abandons branch, only final successful iteration merges to main

This keeps active state clean while preserving history in both the archive and git branches.

#### What We Preserve

Regardless of invalidation:
- All iteration directories (prompts, conversations, validation logs)
- All artifacts ever produced
- Full git history on abandoned branches

This enables debugging, learning from attempts, and potentially resuming abandoned work.

### Key Operations

| Operation | How |
|-----------|-----|
| List running loops | `SELECT * FROM loops WHERE status='running'` |
| Find artifacts from loop | `ls <loop-id>/current/artifacts/` |
| Stream logs | `tail -f <loop-id>/stdout.log` |
| Resume failed loop | Query TaskStore, continue from current iteration |
| See conversation | `cat <loop-id>/iterations/NNN/conversation.jsonl` |
| Find children | `SELECT * FROM loops WHERE parent_loop='...'` |
| Detect stale children | Parse iteration from `triggered_by`, compare to parent's `iteration` |
| Debug iteration N | `ls <loop-id>/iterations/NNN/` |

### Why TaskStore for Metadata

1. **Fast queries**: "Find all running spec loops" is SQLite, not directory scan
2. **Git-friendly**: JSONL files merge cleanly when multiple agents work concurrently
3. **Relationship tracking**: Query children by `parent_loop`, detect stale via `triggered_by` path
4. **Conflict resolution**: Timestamp-based auto-resolution for concurrent updates
5. **Regenerable**: If SQLite corrupts, rebuild from JSONL

## Alternatives Considered

### Alternative 1: Nested Directory Structure

- **Description:** Nest loop directories by hierarchy: `<plan-id>/<spec-id>/<phase-id>/<ralph-id>/`
- **Pros:** Visual hierarchy in filesystem
- **Cons:** Deep paths, hard to query across levels, moving files on invalidation is complex
- **Why not chosen:** Flat structure with TaskStore relationships is simpler and more queryable

### Alternative 2: Single Database (No TaskStore)

- **Description:** Use a dedicated `loopr.db` SQLite database for all metadata
- **Pros:** Simpler single-file storage
- **Cons:** Not git-friendly, no merge driver, loses TaskStore's JSONL audit trail
- **Why not chosen:** TaskStore's JSONL+SQLite pattern provides git-friendly collaboration and audit trail

### Alternative 3: Mark Invalidated (Don't Archive)

- **Description:** Set `status: invalidated` but keep loops in place
- **Pros:** Simpler, no file moves
- **Cons:** Active directory accumulates stale data, harder to see what's current
- **Why not chosen:** Archive keeps active state clean while preserving history

## Technical Considerations

### Dependencies

- **TaskStore**: `~/repos/scottidler/taskstore/` - JSONL+SQLite metadata storage
- **Git**: Branch-per-iteration strategy for code isolation
- **TUI**: Existing conversation storage (referenced by `conversation_id`)

### Performance

- SQLite queries for loop status: O(log n) with indexes
- Directory scans avoided for common operations
- Symlink `current` enables O(1) access to latest iteration

### Security

- All data under `~/.loopr/` (user home, not repo)
- No secrets in loop records (only references)
- Git branches may contain sensitive code - same security model as regular development

### Testing Strategy

- Unit tests for TaskStore record operations
- Integration tests for loop lifecycle (create → iterate → validate → complete)
- Cascade tests for invalidation (parent re-iterates, verify children archived)
- Recovery tests (rebuild SQLite from JSONL)

### Rollout Plan

1. Prototype directory structure with manual loop creation
2. Implement core loop execution with proper file layout
3. Wire up TaskStore for metadata
4. Integrate with TUI for visualization
5. Add invalidation/archive logic

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Branch explosion from many iterations | Medium | Low | Periodic cleanup of merged/abandoned branches |
| Archive directory grows unbounded | Medium | Medium | Add retention policy, compress old archives |
| Stale detection fails (parsing iteration from path) | Low | High | Validate path format on write, add explicit field if needed |
| Concurrent loop modifications corrupt state | Low | High | TaskStore's conflict resolution + file-level atomicity |
| In-progress children when parent re-iterates | Medium | Medium | Signal children to stop, wait for graceful shutdown, then archive |
| Symlink `current` points to deleted iteration | Low | Low | Never delete iterations; archive moves entire loop dir |

## Open Questions

All resolved - see decisions below.

## Decisions Log

| # | Question | Decision |
|---|----------|----------|
| 1 | Global Structure | Flat: `~/.loopr/<project>/loops/<loop-id>/` |
| 2 | Artifact Storage | Inside iteration dirs, `current` symlink for latest |
| 3 | Invalidation | Archive + git branches |
| 4 | Finding Parent's Artifacts | TaskStore relationships (the connective tissue) |
| 5 | Conversation Tracking | Reference (`conversation_id`) to TUI conversation |
| 6 | Loop ID Format | Timestamp-based |
| 7 | Database Schema | TaskStore (JSONL + SQLite cache) |

## References

- [Ralph Wiggum Technique](https://ghuntley.com/ralph/) - Geoffrey Huntley
- [Otto Task Runner](https://github.com/scottidler/otto/) - Directory structure inspiration
- [TaskStore](~/repos/scottidler/taskstore/) - JSONL+SQLite metadata storage

## Next Steps

1. Prototype the directory structure with a simple example
2. Implement the core loop execution with proper file layout
3. Wire up the TUI to read from this structure
