# Loopr Domain Hierarchy

## Overview

Loopr decomposes a goal into a strict hierarchy of planning artifacts,
each level more concrete than the last, bottoming out at executable
coding tasks whose outputs flow through a review-and-integration
pipeline.

```
Plan
 └─ Spec (1+)
     └─ Phase (1+)
         └─ Work (1+)
             └─ Bundle (0+)
                 └─ Tick (aggregates accepted Bundles)
```

## Levels

### Plan

The top-level goal. A single sentence or paragraph describing what the
system should build.

> "Build a vanilla JavaScript CLI todo app with localStorage persistence."

**Cardinality:** one active Plan per session.

### Spec

A design document that elaborates the Plan into a technical approach —
data models, architecture, dependencies, acceptance criteria.

**Cardinality:** 1 Plan → 1+ Specs.

### Phase

An implementation milestone within a Spec. Phases are ordered and
represent coarse-grained stages of work.

**Cardinality:** 1 Spec → 1+ Phases (executed in order).

**Example phases for a todo app Spec:**

1. Core Data Model and Storage Layer
2. CRUD Operations and CLI Interface
3. Error Handling, Edge Cases, and Polish

### Work

The atomic unit of assignable coding work. A Work item targets specific
files (resource tags), declares acceptance criteria, and may depend on
other Work items. Each Work item gets its own git worktree and its own
Implementer agent.

**Cardinality:** 1 Phase → 1+ Work items (may execute in parallel if
no dependency edges exist).

**Example Work items under Phase 1:**

- "Implement Todo model class" — resource tag: `src/todo.js`
- "Implement storage helper" — resource tag: `src/storage.js`
- "Add unit tests for model and storage" — depends on the above two

### Bundle

The output artifact of an Implementer working on a Work item. A Bundle
is the implementer saying *"here is what I built"* — it carries a
branch name, claims (what was done), touched paths, and verification
notes. Think of it as a pull request proposed against that Work.

**Cardinality:** 1 Work → 0+ Bundles.

An implementer may propose multiple Bundles during a single run (e.g.,
one mid-run when it believes the code is ready, and another
force-proposed at `max_iterations`). Only the best one needs to survive
review.

**Bundle lifecycle:**

```
Proposed → Triaged → Reviewed → Accepted → Integrating → Merged
                                                      \→ Rejected
```

Rejection can happen at any non-final state. Superseded marks a Bundle
replaced by a newer one.

### Tick

A published snapshot of the codebase. The Integrator (a deterministic,
non-LLM agent) collects Accepted Bundles, merges their branches, runs
validation commands (test, clippy, fmt), and publishes a Tick if
everything passes.

**Cardinality:** 1 Tick aggregates 1+ accepted Bundles.

## Status FSMs

Each level has its own finite state machine governing transitions.
Transitions are role-gated — only specific agent roles can perform
specific transitions.

| Level  | Key statuses                                       |
|--------|----------------------------------------------------|
| Plan   | Draft → Active → Complete                          |
| Spec   | Draft → Active → Complete                          |
| Phase  | Draft → Active → Complete                          |
| Work   | Draft → Ready → InProgress → InReview → Integrated → Done |
| Bundle | Proposed → Triaged → Reviewed → Accepted → Integrating → Merged |
| Tick   | Created → Sealed → Validating → Published / Failed |

## Agent Roles

| Role         | Operates on    | Description                              |
|--------------|----------------|------------------------------------------|
| Coordinator  | Plan/Spec/Phase/Work | Plans hierarchy, assigns agents, triages Bundles |
| Implementer  | Work → Bundle  | Writes code in a worktree, proposes Bundles |
| Reviewer     | Bundle         | Reviews proposed Bundles for correctness |
| Researcher   | any scope      | Searches codebase for context            |
| Integrator   | Bundle → Tick  | Merges accepted Bundles, runs validation |

## Key Invariants

- One active Implementer per Work item (dedup guard in `agent.start`).
- One active Researcher per target ID (dedup guard in `agent.start`).
- Bundle-aware handback: when an Implementer fails, if it produced a
  usable Bundle the Work transitions to `InReview` (not `Blocked`).
- Work items within a Phase respect dependency edges — a Work blocked
  on another cannot start until the dependency reaches `Done`.
