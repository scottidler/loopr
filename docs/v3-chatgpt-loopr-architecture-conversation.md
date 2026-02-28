# Loopr Architecture Discussion

## You said:

I want to have an architecture discussion about this repo

GitHub - steveyegge/beads: Beads - A memory upgrade for your coding agent https://github.com/steveyegge/beads

Review the details especially around git worktrees and jsonl and sqlite3 and how those tools are woven together architecturally. I am less interested in the specific details that are specific to gastown. I think the abstractions between these two repos is not clean.

## ChatGPT said:

Here's how *Beads weaves git worktrees + JSONL + SQLite (and now Dolt) together*, and where the seams tend to rip—especially if you're trying to keep it "repo-agnostic" and not Gastown-shaped.

### What Beads is actually doing today (conceptually)

#### 1) Three representations of "issue state"

Beads effectively has *three layers* that can represent the same conceptual issue graph:

1. **Primary, queryable store (now Dolt)**
   - The project's main persistent store is a *Dolt database under `.beads/dolt/`* (gitignored).
   - This gives SQL queries, indexes, and *version-aware* operations (diff/history/merge) at the DB level.

2. **Git-portable interchange format (JSONL)**
   - `.beads/issues.jsonl` is treated as the *Git-friendly, line-oriented canonical interchange*: one JSON object per line, designed for diffs/merges/streaming.
   - Hooks export/import between DB <-> JSONL at key Git lifecycle points (pre-commit export, post-merge import, etc.).

3. **Local-only ephemeral store (SQLite)**
   - There's a *separate SQLite file* (`.beads/ephemeral.sqlite3`) for transient issue types ("wisps/molecules") that explicitly *should not* bloat Dolt history and *are not exported to JSONL*.

That "ephemeral store" is the place where SQLite3 still matters architecturally (even after the Dolt migration).

**Key takeaway:** JSONL is not just a backup; it's the *Git transport/interchange* layer, while Dolt is the primary persistent DB, and SQLite is a deliberately non-replicated sidecar for short-lived state.

---

### How Git worktrees are woven in

#### 2) Two different "worktree" concepts exist, and they're easy to conflate

Beads docs explicitly call out that *Beads itself creates worktrees* for a "sync branch" workflow—*separately* from user-created worktrees for parallel development.

##### A) User-created worktrees (parallel dev)

- Beads aims for *one shared `.beads` directory* in the *main repo*, discovered from any worktree.
- So multiple worktrees share the same DB and see the same issue state.

##### B) Beads-created worktrees (sync-branch mode)

When you configure a protected/sync branch, Beads creates an internal worktree:

- lives under `.git/beads-worktrees/<branch>/`
- uses *sparse checkout* containing basically only `.beads/`
- exists so Beads can commit issue updates to the sync branch *without switching your working directory off your current branch*.

This is clever, but it creates sharp edges:

- Git prevents a branch being checked out in two worktrees; users can hit "branch already checked out" errors if the sync worktree is sitting on `main` or another branch.

**Architectural smell:** "sync branch" is implemented via *Git worktree manipulation*, which is a very repo-mechanics-heavy behavior to embed into the storage/sync core.

---

### Where JSONL + worktrees + "DB truth" get tangled

#### 3) The core tension: "Is Git the database, or is the DB the database?"

Beads is straddling two mental models:

- **Git-as-source-of-truth** (JSONL is canonical; DB is a cache)
- **DB-as-source-of-truth** (Dolt is canonical; JSONL is interchange/portability)

The docs now openly describe the Dolt-first architecture. But other material still frames it like "JSONL + SQLite cache".

That's not just doc drift: it reflects that the system still has *two synchronization surfaces*:

- *Git operations trigger JSONL export/import*
- *Dolt operations trigger commits/merges at the DB level*

In worktrees, this gets worse because:

- You're often not in the "main repo root"
- Hooks may run in different worktree contexts
- Sync-branch mode introduces a *third* repo location (the internal sparse worktree)

---

### The abstraction boundary that's probably "not clean" between Beads and Gastown

Based on the Beads worktree + sync-branch design, the likely seam problems (and what I'd call out in an architecture review) are:

#### Problem 1: Git plumbing is *inside* the sync/storage pathway

When your sync mechanism *creates worktrees*, writes into `.git/...`, and commits on your behalf, you've mixed:

- **Domain logic** (issues/deps/events)
- **Storage logic** (DB transactions)
- **Transport logic** (how state moves between machines)
- **Repo mechanics** (worktrees, sparse checkout, branch locking rules)

That makes it hard to share abstractions with another repo unless that repo shares the same Git workflow assumptions.

#### Problem 2: "RepoContext" is doing too much

Beads' worktree doc mentions "automatic discovery" and "path resolution" logic to find the shared DB regardless of which worktree you're in. That sort of logic tends to metastasize into a god-object in multi-repo situations (Gastown tends to do this too), because *everything* needs to know "where am I, what branch, what root, where is `.beads`".

#### Problem 3: Ephemeral SQLite store is "sidecar state" with different lifecycle guarantees

Ephemeral issues are explicitly excluded from JSONL export and sync. If Gastown has its own notion of ephemeral agent/session state, you can easily end up with two overlapping "temporary state" systems that don't share lifecycle rules.

---

### A cleaner set of abstractions (what I'd propose in the discussion)

If you want Beads<->Gastown integration to feel clean, you want **hard boundaries** like:

#### 1) `IssueStore` (pure CRUD, no Git, no worktrees)

- Interface for create/update/query/issues/deps/events.
- Implementation: DoltStore + EphemeralStore routing (already conceptually there).

#### 2) `InterchangeCodec` (JSONL only)

- Responsible for:
  - serialize/deserialize
  - tombstones/deletions markers
  - atomic write patterns
- No git commands. Just file I/O and schema evolution.

#### 3) `SyncOrchestrator` (policy)

- Decides *when* to export/import and in what direction:
  - on startup
  - on hook trigger
  - on explicit `bd sync`
  - in sync-branch mode, where "writes" go
- This is where "Git triggers" belong, but...

#### 4) `GitAdapter` (mechanics)

- Encapsulates:
  - detecting repo root / worktree root
  - committing JSONL changes
  - installing hooks
  - merge driver config
  - *creating internal worktrees (if you keep that design)*
- The point is: Beads core shouldn't "know" how you achieve a sync-branch commit; it should request "commit these files to branch X", and the adapter does it.

Right now, Beads' worktree creation for sync-branch is described as a core behavior. Moving that behind `GitAdapter` is the kind of change that makes it reusable outside the Gastown worldview.

---

### The specific "worktrees + jsonl + sqlite3" story, simplified

If you need a single clean mental model to align teams:

- **Dolt is the authoritative store for persistent issues** (fast SQL, version semantics)
- **JSONL is the portability layer for Git collaboration** (text merge surface, hooks-driven)
- **SQLite is intentionally *not* part of collaboration**; it's for ephemeral/local workflow state only
- **Worktrees are just alternate working directories** that should resolve to the same `.beads` root; sync-branch mode adds an internal sparse worktree for committing without disturbing your current checkout

---

### If you want, I can make this concrete

If you tell me what you mean by "the abstractions between these two repos" (e.g., *what integration points exist today*: shared packages? CLI calls? env vars? filesystem coupling?), I can map:

- which responsibilities belong in Beads vs Gastown,
- what to extract into a thin shared library (or not),
- and which coupling points are most worth cutting first.

But even without that, the big red flag I'd bring into the architecture discussion is:

> **Sync-branch-by-internal-worktree is a powerful hack, but it's repo-mechanics inside the sync core. If Gastown has different opinions about branching/worktrees, you'll never get a clean abstraction until that behavior is behind a Git adapter boundary.**

---

## You said:

Ok thanks for that writeup, now compare what you see there to GitHub repo scottidler/taskstore

## ChatGPT said:

Here's a side-by-side *architectural comparison* between *steveyegge/beads* (what you already looked at) and *scottidler/taskstore*, focusing on how they use *JSONL + SQLite + Git* and where their abstractions differ (especially around things like Dolt, git worktrees, sync logic, and where responsibilities live).

---

### High-Level Purpose

| Repo | Goal |
|------|------|
| **Beads** | A "memory upgrade" for coding agents; general state & event management integrated with git workflows; supports Dolt backend and advanced Git worktree sync flows. |
| **TaskStore** | A simpler, *Beads-inspired storage system*: JSONL is source of truth, SQLite is a cache, and Git merge drivers + hooks enable collaboration/versioning for generic record types in Rust. |

---

### Core Storage Model

Both repos use similar *basic building blocks*, but with **different architectural intentions:**

#### JSONL: Source-Of-Truth Text Format

**Beads**
- JSONL is used as an interchange layer that integrates with *Dolt* and Git. It's not the only truth: Dolt can be the primary store in newer designs.

**TaskStore**
- JSONL *is the canonical source of truth*.
- Append-only JSONL files (one per collection) capture every record change.
- On merge, a custom merge driver handles three-way JSONL merge semantics (based on timestamps).

> **Difference:** Beads now leans toward Dolt as the ultimate store with JSONL as an export/import surface, whereas TaskStore treats JSONL as the *source of truth directly managed by Git*.

---

#### SQLite: Cache / Queryable State

**Beads**
- Uses SQLite for indexing and fast local query, but also supports Dolt for durable SQL with version control semantics.

**TaskStore**
- SQLite is strictly a **cache derived from JSONL**:
  - On sync, all JSONL files are read and latest versions inserted.
  - SQLite reflects only current state with indices defined at compile time.

**Key design line:**

```
JSONL (truth) --> SQLite (fast read cache)
```

TaskStore's pattern is simpler and **one-directional** (write to JSONL, then SQLite), whereas Beads' newer designs involve two storage abstractions (SQLite *and* Dolt backend as alternative persistent store).

---

### Git Integration & Collaboration

#### Merge Drivers & Conflict Resolution

**Beads**
- Uses Git merge drivers and also has sync workflows that interact deeply with Git (worktrees, branches).
- The newer Dolt backend offers SQL-level version control and conflict resolution.

**TaskStore**
- Implements a **custom Git merge driver** specifically for JSONL files:
  - It reads ancestor/ours/theirs JSONLs, groups by ID, and resolves based on `updated_at` timestamps.
  - Most conflicts are auto-resolved; exact ties produce conflict markers.

This is stylistically similar to Beads' custom merge approach for JSONL, but TaskStore's is *focused solely on JSONL* and not part of a broader backend ecosystem.

---

#### Git Hooks / Sync Mechanisms

**Beads**
- Heavy use of Git hooks, **sync branches, and worktrees** to coordinate DB export/import on branch changes and merges (especially with Dolt).
- Worktree logic can be complex because it must coordinate with sparse checkouts and internal sync workflows.

**TaskStore**
- Provides Git hooks *automatically*:
  - pre-commit
  - post-merge
  - post-rebase
  - pre-push
  - post-checkout
- Hooks call `taskstore sync` to regenerate the SQLite cache from JSONL.

**TaskStore**'s hooks are simpler: they don't involve multi-worktree sync branches or alternate branches behind the scenes -- just regenerate the SQLite state from JSONL.

---

### Schema & Record Abstractions

**Beads**
- More domain-specific: event graphs, agents, issue state, wisp/molecule abstractions, and deeper integrations with Gastown workflows.

**TaskStore**
- Very generic:
  - Users declare a type implementing a `Record` trait with:
    - `id`
    - `updated_at`
    - `indexed_fields`
  - TaskStore then handles storage, merge, indexing, and filtering.

This separation makes TaskStore easier to reason about as a library for *any JSONL record store*, whereas Beads embodies a richer domain.

---

### Architectural Boundaries

#### Where Beads is less clean

Beads' architecture tends to mix:

- **State management**
- **Git mechanics (worktrees & sync branches)**
- **Distributed sync logic**
- **Multiple storage backends (Dolt + SQLite)**

This makes abstraction harder when you want to reuse only parts (e.g., just JSONL + cache) because the sync logic is entangled with Git workflows.

#### Where TaskStore is simpler

TaskStore cleanly divides:

```
JSONL (log) --(merge driver + Git)--> JSONL
JSONL --(sync)--> SQLite
```

- No alternative backends
- No sync branches or worktree mechanics
- Merge logic focused exclusively on JSONL
- SQLite cache regenerated from JSONL on hooks

That **simpler pipeline** makes the abstraction between persistent state (JSONL + Git) and transient fast query (SQLite) much clearer and easier to extract for other use cases.

---

### Summary of Architectural Differences

| Concern | Beads | TaskStore |
|---------|-------|-----------|
| JSONL role | Interchange + sometimes source of truth | Source of truth |
| SQLite role | Cache + optionally Dolt backend | Cache only |
| Primary persistent backend | Dolt (optional), JSONL, SQLite | JSONL |
| Git integration | Complex (worktree + sync branch + hooks) | Focused: merge driver + hooks |
| Abstraction boundaries | Mixed across sync/Git/storage | Clean: JSONL vs SQLite |
| Domain specificity | Rich, tied to agents/issues | Generic record store |

---

### Takeaway

If your goal is *clean separation of state storage vs offline collaboration*, **TaskStore's architecture is more clearly delineated**:

- JSONL is the source of truth.
- SQLite is a cache.
- Git merge driver handles collaboration.
- Git hooks keep things in sync.

Beads is more powerful and richer but mixes **repo mechanics (worktrees/branches) with storage logic**, which makes it harder to reuse parts in other contexts without adopting the same workflow assumptions.

---

## You said:

Now go look at scottidler/loop. Read the docs, this thing is not complete or even working correctly. But focus on what the docs say. I am trying to build my own controlled version of gastown. It's going to be a tui app that implements the Ralph wiggum loop (each iteration is a brand new context) but for several levels of product development. Plan, Spec, Phase and Code. The question comes, is TaskStore well suited for this purpose?

## ChatGPT said:

I tried to "go look at `scottidler/loop`", but I can't actually find a public repo at that exact path, and the two close matches that *are* public (`scottidler/loopr` and `scottidler/ralph-wiggum-loop`) currently don't expose meaningful docs/content beyond a one-line README in the raw view. So I can't responsibly summarize "what the docs say" for `scottidler/loop` from public sources.

That said, I *can* answer your real design question: **is TaskStore well suited as the persistence layer for a controlled Gastown-ish, multi-level Ralph loop TUI (Plan -> Spec -> Phase -> Code) where each iteration is a fresh context?**

### Is TaskStore a good fit?

**Yes--for the "state spine."** TaskStore is very aligned with a Ralph-style workflow because it's explicitly designed as:

- **JSONL as git-friendly source of truth** (append-only history; latest wins)
- **SQLite as a derived cache** you can blow away and rebuild (fast queries for a TUI)
- **Git merge driver + hooks** to make collaboration and branch switching survivable

For a TUI that needs to show "what's next", "what changed", "what's blocked", "what iteration are we on", etc., this architecture is a strong match.

#### Why it matches your "fresh context every iteration" goal

In a Ralph loop, you want **durable progress outside the chat context**. TaskStore gives you a durable, diffable, mergeable event-ish log (JSONL) and a fast read model (SQLite) that your TUI can query constantly.

Concretely: each iteration can write a small set of records (iteration result, new/updated tasks, updated status, learnings), and the next iteration can rebuild a "context pack" from the latest state.

### Where TaskStore is *not* enough (and what you'll likely want to add)

TaskStore's own README calls out limitations that matter for an always-on iterative loop:

1. **Timestamp-based conflict resolution**
   - Merge uses newest `updated_at`; same timestamp can yield conflicts; it assumes reasonably synced clocks.
   - For agent loops (multiple processes, multiple machines), clock skew will bite you eventually.
   - *Mitigation:* add a monotonic tie-breaker (ULID, Lamport clock, `(device_id, counter)`) into every record and update merge rules to use it when timestamps tie or drift.

2. **Unbounded JSONL growth + no compaction**
   - JSONL grows forever; no compaction yet.
   - Ralph loops produce lots of small state changes; you'll accumulate noise fast.
   - *Mitigation:* periodic compaction command (rewrite "latest only" into a new JSONL + archive the old log) or "segment per day/iteration" files.

3. **Full sync on merge (not incremental)**
   - Rebuilds SQLite from JSONL on git events; currently "full sync on every merge."
   - That's fine for small stores, but a multi-level product dev system with many iterations will grow.
   - *Mitigation:* incremental import (track last applied offset per JSONL file).

4. **Indexes are compile-time**
   - You can't add indexes dynamically without code changes.
   - For a product-dev TUI you'll keep discovering new query needs.
   - *Mitigation:* decide on a minimal but sufficient index set up front (status, parent links, level, iteration_id, "active" flags, priority, owner), and accept occasional schema bumps.

### How I'd model your Plan -> Spec -> Phase -> Code stack in TaskStore

TaskStore shines if you keep each record type simple and link them by IDs.

Suggested collections (each a `Record`):

- `plans` (top-level product goals/themes)
- `specs` (requirements + acceptance criteria)
- `phases` (milestones / execution stages)
- `code_tasks` (granular build/test/refactor items)
- `iterations` (one row per Ralph loop iteration; stores summary, outcome, pointers to artifacts)
- `artifacts` (paths to files: generated spec docs, patch refs, logs)
- `learnings` (short durable facts the next iteration needs)

Then your TUI can:

- Render current tree: Plan -> Spec -> Phase -> Code tasks
- Generate the next iteration's "fresh context" by querying:
  - active plan/spec/phase
  - top N ready code_tasks
  - last iteration's failures/learnings
  - acceptance criteria status

### Bottom line

**TaskStore is well-suited as the persistence layer** for your controlled Gastown-ish Ralph TUI--*as long as you treat it as the "state backbone," not the whole system*.

You'll likely need to extend it (or wrap it) for:

- better-than-timestamp merge semantics (monotonic IDs)
- compaction / segmentation
- incremental sync for scale
- a clean "context pack builder" that produces the minimal prompt bundle per iteration

---

## You said:

Yeah my bad I don't think I have pushed my code or docs to that repo yet

## ChatGPT said:

No worries -- that actually makes this a *better* conversation. You're early enough to avoid painting yourself into a corner.

Let's reason from first principles.

You're building:

- A **controlled Gastown-like system**
- TUI-based
- Implements the **Ralph Wiggum Loop**
- Each iteration = completely fresh LLM context
- Structured across levels:
  - **Plan**
  - **Spec**
  - **Phase**
  - **Code**

The core question:

> Is TaskStore the right persistence model for this?

---

### Step 1: What Your System Actually Needs

Your loop has very specific properties:

#### Stateless agent iterations

Every iteration:

- Reads durable state
- Produces new state
- Writes durable state
- Dies

This means you need:

- Durable structured state
- Deterministic rebuild of context
- Append-safe collaborative storage
- Git-friendly diffs
- Fast queries for TUI

That is exactly the kind of thing JSONL + SQLite is good at.

---

### Step 2: Is TaskStore Conceptually Aligned?

TaskStore gives you:

```
JSONL (source of truth, git merged)
    |
SQLite (rebuilt read model)
    |
TUI queries
```

That aligns very well with:

```
Iteration N:
    read SQLite
    build prompt
    generate changes
    append JSONL
    sync
    rebuild SQLite
```

Architecturally, that's clean.

So yes -- **TaskStore is well suited as the persistence spine**.

But that's not the whole story.

---

### Step 3: Where TaskStore Is Insufficient for Ralph Loop

Your system is not just a record store.

It is:

- A structured multi-level development orchestrator
- With iteration-scoped memory
- With explicit promotion between levels (Plan -> Spec -> Phase -> Code)
- With controlled context assembly

TaskStore does NOT give you:

#### Hierarchical semantic relationships

It stores records, but not behavior.

You'll need:

- parent relationships
- lifecycle transitions
- promotion logic
- readiness logic
- "what is active?" logic

That lives above TaskStore.

---

#### Iteration lineage semantics

A Ralph system needs:

- Iteration ID
- Iteration parent
- Snapshot of what was fed into prompt
- Outcome classification
- Failure tagging

TaskStore can store this, but it doesn't enforce or model it.

You'll need a domain layer like:

```rust
struct Iteration {
    id,
    level,
    input_snapshot_hash,
    output_summary,
    status,
    parent_iteration_id,
}
```

---

#### Deterministic Context Builder

This is the most important missing piece.

Your system needs:

```rust
build_context(level, iteration_id) -> PromptBundle
```

That function:

- queries active plan/spec/phase
- selects next ready tasks
- attaches relevant learnings
- includes acceptance criteria
- enforces token budget

TaskStore only gives you storage.

The context-builder is your real engine.

---

### Step 4: The Real Architectural Question

The real question is not:

> Is TaskStore good enough?

The real question is:

> Should TaskStore be the core, or should it be just a persistence adapter behind a domain engine?

The correct answer for your goals:

**TaskStore should be a persistence adapter, not the brain.**

---

### Step 5: Where TaskStore Actually Shines Here

It is especially good for:

#### Git-native collaboration

You want:

- branches per feature
- loop iterations committed
- inspectable diffs
- conflict resolution

JSONL + merge driver is excellent for that.

---

#### Deterministic rebuild

If SQLite is always rebuilt from JSONL, you get:

- stateless TUI startup
- crash-safe recovery
- iteration replay capability

That aligns with your "fresh context" philosophy.

---

#### Append-oriented iteration model

Each iteration appends new state.

That maps cleanly to:

- event-like workflow
- immutable history
- rebuildable read model

---

### Step 6: Where You Might Outgrow It

You will outgrow TaskStore if:

1. You need distributed multi-device concurrent loops with strict causality.
2. You need extremely large state (JSONL compaction becomes necessary).
3. You need cross-record transactional guarantees.
4. You want versioned snapshots at query time (Dolt territory).

Right now, for a controlled local-first system?

It's a good fit.

---

### Step 7: The Bigger Architectural Insight

Beads mixes:

- storage
- git mechanics
- domain
- sync
- agent semantics

That's why it feels messy.

If you build yours correctly:

You want a strict layering:

```
+---------------------------+
| Ralph Loop Engine         |
| - iteration lifecycle     |
| - level promotion         |
| - context builder         |
+-------------+-------------+
              |
+-------------v-------------+
| Domain Model              |
| - Plan/Spec/Phase/Code    |
| - readiness logic         |
| - state transitions       |
+-------------+-------------+
              |
+-------------v-------------+
| TaskStore (Persistence)   |
| - JSONL truth             |
| - SQLite read cache       |
| - Git merge driver        |
+---------------------------+
```

If you keep that separation clean, you will avoid Beads' abstraction drift.

---

### Step 8: My Direct Answer

**Yes. TaskStore is well suited for your Ralph-loop TUI -- as long as:**

- It remains just the durable state layer.
- You implement a strong domain layer above it.
- You build a deterministic context assembler.
- You plan early for compaction + better merge semantics.

If instead you try to evolve TaskStore into the orchestrator itself, you'll recreate Beads' complexity.

---

## You said:

Here is my next idea. I was going to use a git workflow per agent. Then in a previous conversation with an llm we discussed tools that needed access to disk, like a git worktree, needs to be process, but agents that were mostly interacting with data output from a tool or output from a search or llm API could be lightweight tokio tasks. My thought when multiple agents were working on the same doc or code that they would have their own git worktrees checkout and as soon as something got merged to a branch then every other agent would be signalled to rebase against this branch (and the new change). Thoughts on all of that?

## ChatGPT said:

This can work, and the split you're describing (heavy "disk/Git" agents as processes + lightweight "reasoning/IO" agents as Tokio tasks) is the right instinct. The big risk is **turning Git into your distributed scheduler** and then discovering you've built a conflict factory.

Here's how I'd think about it.

### 1) Process-per-worktree is the right boundary

Anything that needs:

- a working directory
- `git status/diff/apply`
- running build/tests/formatters
- editing files

...should be a **process** with an exclusive worktree checkout. You avoid a whole class of footguns (cwd races, temp files, editor buffers, partial writes).

Lightweight "thinkers" (LLM calls, search, parsing tool outputs, ranking tasks) can be Tokio tasks because they're mostly CPU/network and can share read-only state.

**Rule of thumb:** if it needs `git` *and* writes files, it's a process.

### 2) "Git workflow per agent" is viable if you add two missing pieces

If every agent has its own branch+worktree, you need:

#### A) A single integration authority

Rebasing everyone whenever anything merges is brutal unless there's a clear "truth branch" and a gatekeeper.

Options:

- **One "integrator" agent/process** that is the only thing allowed to merge to `main` (or `integration`). Others open PR-like proposals.
- Or you enforce **code ownership by file area** so merges rarely touch the same files.

Without this, your system degenerates into:

- constant rebases
- redo work
- half-applied patches
- hard-to-reproduce states

#### B) A stable unit of work (patch queue), not "live edits"

Have agents produce **atomic changesets** (patch + tests + metadata), then integrate those. Don't let them freewheel editing the same doc/code at the same time unless they're editing disjoint regions.

### 3) "Signal every agent to rebase after merge" -- do it, but don't literally rebase immediately

The right pattern is **invalidate -> replan**, not "rebase right now".

When `integration` advances:

- send a signal: "your base is stale"
- the agent finishes its current critical section (or cancels safely)
- then it rebases/merges and re-validates

Otherwise you'll interrupt agents mid-edit and create a lot of wasted work.

#### Practical: prefer `git merge` over `git rebase` in automation

For automation, `merge` is often safer because it preserves history and avoids rewriting commits under your feet. Rebase is fine if you strictly enforce "agent branches are private and ephemeral," but the moment you share them, rebase becomes pain.

If your agents are the only consumers of their branches, rebase is okay.

### 4) Worktrees are great, but you need to manage branch checkout constraints

Git won't allow the same branch checked out in multiple worktrees. So your model should be:

- `integration` checked out in exactly one place (maybe none; just fetch)
- each agent worktree is checked out on **its own branch**
- agents never checkout `integration` in their worktree; they `fetch` + `rebase origin/integration`

Also: have a cleanup strategy for stale worktrees.

### 5) This meshes nicely with TaskStore--if you separate "state sync" from "code sync"

You can treat the world as two sync planes:

#### Plane A: repo contents (code/docs)

- agent worktrees + branches
- merge to integration branch

#### Plane B: workflow state (Plan/Spec/Phase/Code, iterations, outcomes)

- TaskStore JSONL truth + SQLite cache
- updates come from agents as structured records ("I made patch X", "tests passed", "spec updated")

You **do not** want workflow state to be implicitly derived from Git history alone. Store it explicitly in TaskStore, and reference Git SHAs/branches as artifacts.

### 6) Avoid the "two agents edit same doc" trap: use locking or sharding

If multiple agents can touch the same document/code:

- you need either **sharding** (file/section ownership) or **locking**.

Locking can be lightweight:

- a TaskStore record `Lock { resource_id, owner_agent, expires_at }`
- enforced by the orchestrator before granting an edit task

Sharding is often better:

- one agent owns `spec.md`
- others propose edits as patches or comments, not direct edits

### 7) A concrete orchestration pattern that tends to work

**Roles**

- **Coordinator (Tokio task)**: assigns work, tracks locks, watches integration branch.
- **Integrators (process)**: one process that merges to integration, runs tests, updates TaskStore.
- **Workers (process)**: per-agent worktree, produces patches/proposals.
- **Analysts (Tokio tasks)**: LLM/search, generate plans/spec deltas, review diffs, etc.

**Flow**

1. Coordinator assigns a work item (plus a lock if needed).
2. Worker edits in its worktree, runs checks, produces a change bundle:
   - patch/commits
   - rationale
   - affected files
   - "expected" tests
3. Integrator merges bundles onto `integration` and validates.
4. Coordinator broadcasts "integration advanced to SHA X".
5. Workers mark themselves stale; on next safe point, they rebase and re-evaluate.

### 8) The main pitfall: thrash from frequent merges

If integration moves every minute, workers are always stale.

Mitigations:

- batch merges into "integration ticks" (e.g., every N minutes or when M bundles ready)
- only signal workers when the merge touches files they care about (path-based impact)
- prefer additive changes / new files early in the loop

---

#### My take

Your idea is solid, but I'd change one thing:

**Don't force a rebase; force a replan.**
Treat "integration moved" as invalidating the worker's assumptions. Sometimes the right response is "drop this work item" or "recompute patch against new base," not "blindly rebase and continue."

---

## You said:

I think my rebase idea really meant what you said, replan when safe to do so. Step back. You see what I am trying to build, you cautioned about bottlenecks and conflicts. How would you suggest to build something like this that swarms a problem or in parallel and basically models the a development team quickly swarming on building a solution?

## ChatGPT said:

Yep -- if the goal is "simulate a dev team swarming," you want **parallelism where it helps** and **serialization where it must**, with tight feedback loops and minimal merge pain. Here's a design that tends to work in practice.

### The core idea

Split work into two planes:

1. **Thinking plane (high-parallel, cheap to redo)**
   LLM/search/review/ranking, generating options, spec proposals, test plans, code review, risk analysis.
   -> Tokio tasks, no worktrees.

2. **Changing plane (low-parallel, expensive to redo)**
   Editing files, running tests, producing patches.
   -> Process-per-worktree.

Most "swarming" should happen in plane 1. Plane 2 should be carefully metered.

---

### Roles that map to a real dev team

#### 1) Coordinator (product manager + EM)

Single authority that:

- defines "what good looks like" (acceptance criteria)
- breaks the work into small chunks
- assigns chunks + locks/ownership
- decides what gets integrated next
- maintains the "current truth" in TaskStore

This can be one process + a few Tokio tasks.

#### 2) Spec/Design swarm (high parallel)

Multiple lightweight agents producing:

- spec outline + edge cases
- API design alternatives
- migration plan
- test strategy
- risk list
- sequencing proposal

They don't edit the repo. They write structured artifacts into TaskStore.

#### 3) Implementers (bounded parallel)

A small number (2-4) of worktree processes that:

- each owns a slice of the codebase or a bounded change
- produce atomic bundles (commit/patch + tests + explanation)

#### 4) Reviewer swarm (high parallel)

Lightweight agents that:

- review diffs
- search for similar patterns in repo
- check style conventions
- generate "things to verify" lists
- attempt to break the change mentally

Again: no repo writes.

#### 5) Integrator (CI gatekeeper)

One authority that merges to `integration` and runs:

- tests
- lint/format
- minimal sanity

Then advances the integration tick.

---

### The mechanism: "Bundles" + "Ticks"

This is the trick that keeps you from thrashing.

#### Bundles

A bundle is the unit of change:

- patch/commits
- affected paths
- "contracts": what tests/behaviors it claims to satisfy
- dependencies ("requires bundle X first")
- rollback note if it fails

#### Integration ticks

Instead of merging continuously, you merge in discrete ticks:

- tick advances to SHA X
- all workers get "base advanced" signal
- workers replan at safe points

This makes your system stable and observable. It also feels like a team doing short integration cadences.

---

### How to swarm without constant conflicts

#### 1) Aggressively shard the work

Swarming fails when multiple implementers touch the same files.

So default to shards like:

- **new files** first (additive changes)
- **one module per implementer**
- **one doc section per implementer**
- **one test suite per implementer**

When overlap is unavoidable, use explicit ownership:

- one agent edits
- others propose patch hunks or comments

#### 2) Use "proposal mode" for docs/spec

Docs are conflict magnets. For docs, do:

- each agent writes a proposal artifact (structured markdown chunks)
- coordinator merges proposals into the canonical doc (single writer)

This mirrors real teams: lots of comments, one editor.

#### 3) Lock only what truly needs locking

Locks should be on *resources*, not people:

- file path or directory
- "spec section" ID
- "API surface" ID

Use short TTL locks with renewal to avoid deadlocks.

---

### Your Plan -> Spec -> Phase -> Code loop as a state machine

To make the swarm productive, enforce "levels":

#### Plan level

Goal: define objective + success metrics + non-goals
Swarm outputs: options, risks, constraints
Coordinator picks: one plan record becomes active

#### Spec level

Goal: acceptance criteria, interfaces, edge cases
Swarm outputs: spec deltas, test matrix
Coordinator picks: canonical spec revision

#### Phase level

Goal: sequencing + dependencies + milestones
Swarm outputs: work breakdown structure, critical path
Coordinator picks: phases and assigns owners

#### Code level

Goal: bundles + integration ticks
Swarm outputs: bundles + reviews + validation
Integrator merges

The "fresh context each iteration" becomes: **rebuild prompt from state appropriate to that level**.

---

### A concrete operating loop (what happens each iteration)

**Iteration starts**

1. Coordinator reads TaskStore state + repo status (latest integration tick).
2. Chooses level to operate on (Plan/Spec/Phase/Code).
3. Spawns:
   - 5-20 lightweight tasks (think/review/search)
   - 2-4 worktree processes (implement) if in Code level

**Iteration ends**

4. Coordinator collects artifacts:
   - spec proposals / reviews / risk lists
   - bundles from implementers
5. Integrator merges *some* bundles (bounded), runs checks, advances tick.
6. Coordinator writes iteration summary + updated priorities into TaskStore.

This yields the "team swarm" feeling: lots of parallel thought, controlled integration.

---

### Avoiding bottlenecks while keeping correctness

#### Keep the integrator single-threaded

This *is* a bottleneck, but it's the right one. It's your CI gate.

You scale throughput by:

- improving shard quality (less overlap)
- making bundles smaller
- making validation faster (targeted tests, fast linters)
- batching merges per tick

#### Make planning massively parallel

Planning/spec/review are where you can throw concurrency at the problem without paying merge tax.

---

### Where TaskStore fits best in this architecture

TaskStore is great for:

- work items, locks, bundles, iterations
- storing proposals and reviews as records
- tracking integration ticks and staleness
- making the TUI fast (SQLite cache)

Git is great for:

- code and docs truth
- patch application
- integration tick SHAs

You want them linked, not conflated:

- TaskStore record references Git SHAs/branches/artifacts

---

### One strong recommendation

Build the system around a **"work item -> bundle -> tick"** pipeline.

If you do just that, everything else (Plan/Spec/Phase/Code, Ralph loop freshness, swarm behaviors) becomes manageable.

---

## You said:

Sounds good but let's talk about the plan for the whole system you described. Concentrate on the most important details not everything. Draw the connections to the TaskStore and the records you mentioned. Talk about all of the personalities that you described for the different roles. Pay special attention to the ones that require coordination or only one can do while the other communicate and contribute

## ChatGPT said:

Here's the plan I'd build toward, with the *minimum moving parts that matter*, and how it maps to **TaskStore records** and the **role "personalities"**--especially the ones that must be single-authority.

### The core shape of the system

You're building a "dev team in a box" that runs in iterations:

1. **Many contributors propose** (parallel, cheap)
2. **One decider selects** (serial, authoritative)
3. **Few implementers change the repo** (bounded parallel)
4. **One integrator advances the truth branch** (serial, authoritative)
5. Everything is written down in TaskStore so the next iteration can be built from scratch.

That's the whole architecture in one sentence.

---

### The two single-authority roles (non-negotiable)

These are the "coordination bottlenecks" you *want*.

#### 1) Coordinator (PM + EM personality)

**Why only one:** someone must own prioritization, locks, and what the system is doing *right now*.

**What it does**

- chooses the active level: Plan / Spec / Phase / Code
- creates & assigns work items
- grants/denies locks
- decides which proposals become canonical
- decides which bundles get sent to integration this tick
- writes the iteration summary + next priorities

**How it communicates**

- reads contributions from TaskStore
- posts decisions back to TaskStore as "canonical" records
- signals workers: "new work", "stale base", "lock granted/revoked"

**TaskStore records it owns**

- `Iteration` (source of "what happened")
- `Work` (the backlog + assignments)
- `Lock` (resource coordination)
- `Decision` (canonical choices: chosen plan/spec/phase)
- `PriorityQueue` (or simply fields on Work)

This is the role that prevents "swarm = chaos."

---

#### 2) Integrator (Release engineer personality)

**Why only one:** Git truth can only advance coherently through one gate. Otherwise you get merge storms and nondeterminism.

**What it does**

- takes accepted `Bundle`s
- applies/merges them onto `integration`
- runs validations (tests/lint/build)
- if green: advances integration "tick" to SHA X
- if red: rejects bundle(s), records failure reason, requests fixes

**How it communicates**

- writes integration advancement to TaskStore
- broadcasts "tick advanced to SHA X"
- reports bundle outcomes (pass/fail)

**TaskStore records it owns**

- `Tick` (tick number -> git SHA + metadata)
- `BundleStatus` (accepted/merged/rejected, failure info)

This is your "CI gate with a brain."

---

### The bounded-parallel "repo writers"

#### 3) Implementers (Senior engineer personality)

**Why bounded:** writing the repo is expensive and causes conflicts. Keep it to 2-4 concurrent.

**What they do**

- claim a `Work`
- (optionally) acquire a `Lock`
- in their own worktree branch: make changes + tests
- emit a `Bundle` (atomic patch/commits + notes + touched paths)
- never merge to integration directly

**How they communicate**

- they don't debate in the repo; they ship bundles
- they read spec/decisions from TaskStore before coding
- when tick advances, they "replan when safe"

**TaskStore records they produce**

- `Bundle`
- `WorkUpdate` (progress notes, blockers)
- optionally `Artifact` (logs, screenshots, test output)

**Important invariant**

> Implementers are writers; they should not be deciders.

---

### The high-parallel "contributors" (cheap to redo)

These are almost all **Tokio tasks**.

#### 4) Spec/Design swarm (Staff engineer personality)

**What they do**

- propose spec deltas, edge cases, API shapes
- draft acceptance criteria and test matrices
- identify risky areas and dependencies

**What they never do**

- edit canonical spec directly (unless you explicitly allow one "spec editor")
- claim locks on code files

**TaskStore output**

- `Proposal { kind: SpecDelta | DesignOption | TestPlan }`
- `Risk`
- `Question` (things needing a decision)

#### 5) Reviewer swarm (Code reviewer personality)

**What they do**

- review bundles/diffs
- look for breakage, style issues, missing tests
- suggest targeted improvements

**TaskStore output**

- `Review { bundle_id, findings, severity }`

#### 6) Researcher / Searcher (Librarian personality)

**What they do**

- repo search (ripgrep-style), dependency lookup, "where else is this pattern used"
- external web research when needed
- summarize relevant prior art

**TaskStore output**

- `Finding { references, summary }`

These roles are "many voices." They increase quality without increasing merge conflicts.

---

### The TaskStore record model that makes this all click

You don't need a huge schema. You need ~7 record types.

1. **`Iteration`**
   - `id`, `level` (Plan/Spec/Phase/Code)
   - `tick_base_sha` (what it started from)
   - `summary`, `outcome`, `next_steps`

2. **`Work`**
   - `id`, `level`, `title`
   - `status` (Ready/InProgress/Blocked/Done)
   - `assignee_agent_id`
   - `depends_on[]`
   - `resource_tags` (paths/modules/spec sections)

3. **`Lock`**
   - `resource_id` (file path, module, spec section)
   - `owner_agent_id`
   - `expires_at`, `renewable`

4. **`Proposal`**
   - `kind` (PlanOption/SpecDelta/PhaseBreakdown/TestPlan)
   - `target_id` (plan/spec/phase)
   - `content` (structured text)
   - `rationale`

5. **`Decision`**
   - "Coordinator says this is canonical"
   - references proposals or directly encodes the chosen option
   - this is how you prevent bikeshedding forever

6. **`Bundle`**
   - `branch`/`commit_range` or patch ref
   - `touched_paths[]`
   - `claims` (what it satisfies)
   - `requires_tick_sha` (what it was based on)
   - `work_id`

7. **`Tick`**
   - `number`, `integration_sha`
   - `merged_bundle_ids[]`
   - `validation_result`

That's enough to run the whole team.

---

### The most important connections

#### Connection A: Coordinator <-> TaskStore

Coordinator is basically a state machine controller:

- reads: Works, Locks, Proposals, Reviews, Ticks
- writes: Iteration, Decisions, Assignments, Lock grants

#### Connection B: Implementers <-> Git <-> Bundles <-> Integrator

Implementers produce Git reality, but only as **Bundles**.
Integrator is the only one who turns bundles into **Ticks**.

#### Connection C: Everyone <-> Decisions

All contributors can propose; only Coordinator writes Decisions.
Implementers should treat Decisions as "truth."

---

### The communication contract (so roles don't step on each other)

- Contributors **do not DM implementers** with ad-hoc instructions; they write `Proposal`/`Review`.
- Coordinator reads all that and writes a `Decision`.
- Implementers implement the Decision.
- Integrator validates and advances Tick.
- Tick advancement triggers "stale base" signals.

This is how you get swarm throughput without thrash.

---

### A tiny "who can do what" matrix

- **Coordinator:** can assign work, grant locks, choose canon
- **Integrator:** can advance integration branch/tick
- **Implementers:** can write code/docs in their worktrees, produce bundles
- **Swarm roles:** can only write proposals/reviews/findings (no repo writes)

That division is the key to keeping the system sane.

---

## You said:

Ok I like it. Do what you suggested

## ChatGPT said:

Awesome -- here are the **state transitions + invariants** for the 3 core objects that make the whole swarm stable: **Work, Bundle, Tick**. This is the "spine" you can build everything else around.

### 1) Work state machine

#### States

- **Draft**: exists but not ready to assign (missing decision/spec/phase context)
- **Ready**: well-scoped, has acceptance criteria (or points to them), can be picked up
- **InProgress**: assigned to an implementer; may have locks
- **Blocked**: cannot proceed (dependency, missing decision, failing integration, lock conflict)
- **InReview**: bundle submitted; awaiting reviewer swarm + coordinator acceptance
- **Integrated**: merged into integration tick successfully
- **Done**: verified against acceptance criteria (may be same as Integrated if validation is sufficient)
- **Abandoned**: no longer relevant / superseded

#### Allowed transitions

- Draft -> Ready (Coordinator)
- Ready -> InProgress (Coordinator assigns OR implementer claims with coordinator approval)
- InProgress -> Blocked (Implementer or Coordinator)
- Blocked -> Ready (Coordinator after resolving dependency/decision)
- InProgress -> InReview (Implementer submits Bundle)
- InReview -> InProgress (Coordinator requests changes; bundle rejected)
- InReview -> Integrated (Integrator merges accepted bundle)
- Integrated -> Done (Coordinator/Integrator marks verified)
- Any -> Abandoned (Coordinator only)

#### Work invariants (the rules that prevent chaos)

1. **Single assignee in InProgress/InReview**
   - `assignee_agent_id` must be set and exclusive.

2. **Ready implies scope is bounded**
   - Must include at least one of:
     - `acceptance_ref` (Decision/Spec link)
     - `checklist` (inline)

3. **InReview implies there is exactly one "active" Bundle**
   - You can keep history, but one bundle is current.

4. **Dependencies are acyclic (best-effort)**
   - At minimum, prevent obvious cycles at creation time.

5. **Work must declare resource tags**
   - paths/modules/spec-section IDs; used for impact/staleness routing.

---

### 2) Bundle state machine

Think of a Bundle as your PR-like object.

#### States

- **Proposed**: implementer produced it, not yet triaged
- **Triaged**: Coordinator validated it matches intent + has required metadata
- **Reviewed**: reviewer swarm completed (or timed out)
- **Accepted**: Coordinator says "ship it"
- **Integrating**: integrator is attempting merge/apply
- **Merged**: merged into integration (tick advanced or will advance)
- **Rejected**: not going forward (failed tests, wrong approach, conflicts)
- **Superseded**: replaced by a newer bundle for same work item

#### Allowed transitions

- Proposed -> Triaged (Coordinator)
- Triaged -> Reviewed (Coordinator triggers review swarm)
- Reviewed -> Accepted (Coordinator)
- Accepted -> Integrating (Integrator)
- Integrating -> Merged (Integrator)
- Integrating -> Rejected (Integrator)
- Proposed/Triaged/Reviewed -> Rejected (Coordinator)
- Any non-final -> Superseded (Coordinator when a newer bundle replaces it)

#### Bundle required fields (minimum contract)

- `work_id`
- `base_tick_sha` (or base tick number)
- `touched_paths[]`
- `patch_ref` (branch+range or patch artifact)
- `claims` (what it asserts: "adds endpoint X", "fixes bug Y")
- `verification` (what was run: unit tests, lint, command output)
- optional: `locks_used[]` (resource IDs)

#### Bundle invariants (the rules that keep integration sane)

1. **Bundle must declare its base**
   - `base_tick_sha` is mandatory.

2. **Integrator never merges a stale bundle blindly**
   - If `current_tick_sha != base_tick_sha`, integrator must either:
     - rebase/replay bundle automatically and re-run verification, OR
     - reject with reason "stale; please rebase"

3. **At most one Accepted bundle per Work at a time**
   - prevents two parallel "final" proposals.

4. **Bundle cannot touch locked resources it doesn't own**
   - integrate-time enforcement: touched_paths must be compatible with locks.

5. **Verification metadata is required**
   - even if minimal ("not run; reason: ...") -- prevents silent low-quality merges.

---

### 3) Tick state machine (integration cadence)

A Tick is a discrete advancement of the shared truth branch.

#### States

- **Open**: collecting candidates (accepted bundles)
- **Sealing**: integrator picked a set, is about to apply/merge
- **Validating**: tests/lint/build running
- **Published**: tick is the new truth (integration SHA recorded)
- **Failed**: tick attempt failed; nothing advanced, reasons recorded

#### Allowed transitions

- Open -> Sealing (Integrator)
- Sealing -> Validating (Integrator)
- Validating -> Published (Integrator)
- Validating -> Failed (Integrator)
- Failed -> Open (Integrator starts a new tick)

#### Tick invariants

1. **Only one tick can be in Sealing/Validating at a time**

2. **Published tick has exactly one integration SHA**

3. **Tick records exactly which bundles it attempted**
   - `attempted_bundle_ids[]`
   - `merged_bundle_ids[]` (subset)

4. **A tick failure must be attributable**
   - record failing command/output artifact IDs

5. **Publishing a tick triggers staleness events**
   - agents with in-progress work touching affected paths become "stale" (replan flag)

---

### 4) The "staleness + replan" rule (the safe rebase you meant)

When a tick is published:

- For every Implementer with `Work` in `InProgress` or `InReview`,
- If `touched_paths` intersect with that work item's `resource_tags` (or any open locks),
- mark their work item as **StaleBase = true** (a field, not a state).

**Policy:** Implementer must acknowledge and replan at the next safe point:

- before submitting a bundle
- before acquiring/renewing locks
- after finishing a local test run
- before applying another patch

This is how you get "rebase when safe" without thrash.

---

### 5) Minimal coordinator + integrator algorithms (behavioral invariants)

#### Coordinator (single authority)

- Never move a Work to Ready unless:
  - it has acceptance criteria reference
  - it has resource tags
- Never allow two implementers to hold overlapping locks unless explicitly allowed.
- Never accept a bundle without:
  - base tick
  - verification metadata
  - touched paths declared

#### Integrator (single authority)

- Only integrates Accepted bundles.
- Refuses bundles that:
  - are stale AND can't be cleanly replayed
  - violate locks
  - fail validation
- Publishes ticks at a steady cadence (batching) to reduce thrash.

---

### 6) What you can implement first (MVP order)

If you build just these in order, you'll have a working swarm:

1. TaskStore records: Work, Bundle, Tick, Lock, Iteration
2. Coordinator: create/assign/lock/accept bundle
3. Implementer process: worktree -> propose bundle
4. Integrator process: merge bundles -> run tests -> publish tick
5. Staleness marking + "replan required" gate before bundle submit

Everything else (spec swarm, review swarm, plan/spec/phase levels) plugs into this backbone.

---

## You said:

Speak to me about how the Ralph wiggum loop with a fresh context + learnings is threaded for all of these different personas

## ChatGPT said:

Good. This is the heart of it.

If you get the **Ralph Wiggum loop threading** right across personas, the whole system becomes coherent instead of chaotic.

Let's restate the Ralph principle:

> Each iteration runs with a **fresh LLM context**, rebuilt deterministically from durable state, plus selected "learnings" from prior iterations.

The challenge is:
You don't have one Ralph. You have **multiple personas**.

So the question becomes:

> How do multiple Ralphs share durable intelligence without sharing context?

---

### First Principle: Context Is Ephemeral, State Is Canonical

Every persona iteration should follow this structure:

```
1. Load canonical state from TaskStore + Git
2. Select relevant learnings
3. Build minimal role-specific prompt
4. Produce output (Proposal, Bundle, Review, Decision, etc.)
5. Persist output as structured record
6. Context dies
```

No persona keeps memory.
Only TaskStore remembers.

That's the key architectural invariant.

---

### What "Learnings" Actually Mean

A learning is not chat history.

A learning is:

- A durable fact discovered during work
- A correction to a false assumption
- A pattern discovered in the repo
- A failure mode
- A policy refinement

Learnings should be structured, not prose dumps.

Example:

```
Learning {
  scope: "auth module"
  level: Code
  statement: "All endpoints require tenant_id, not org_id"
  source_iteration: 42
  confidence: High
}
```

Or:

```
Learning {
  scope: "Spec: API"
  statement: "Clients rely on snake_case in JSON"
}
```

These are tiny, high-signal pieces.

---

### Now Thread Ralph Across Personas

Each persona has its own Ralph loop:

But they differ in:

- What state they read
- What learnings they care about
- What artifacts they produce

Let's walk role by role.

---

### 1) Coordinator Ralph (Meta-Ralph)

This is the most important one.

**What it loads**

- Current active level (Plan/Spec/Phase/Code)
- Open Works
- Current Tick
- All Decisions
- High-confidence Learnings
- Bundles awaiting triage

**What it ignores**

- Raw chat history
- Detailed diffs (unless needed)
- Low-confidence speculative learnings

**What it produces**

- Assignments
- Decisions
- Priorities
- Iteration summary
- New Learnings (process-level)

**What learnings matter to it**

Process learnings:

- "Large bundles cause merge failures"
- "Spec ambiguity caused 3 reworks"
- "Lock conflicts are increasing"

Coordinator Ralph improves the *system itself*.

---

### 2) Implementer Ralph (Engineer Ralph)

This is the one most people imagine.

**What it loads**

- Work
- Canonical Spec/Decision
- Relevant Learnings scoped to:
  - affected paths
  - module tags
  - API contracts
- Current integration SHA

**What it ignores**

- Unrelated spec proposals
- Other Works
- Global backlog noise

**What it produces**

- Bundle
- WorkUpdate
- Code-level Learnings

**Example Code Learnings**

- "Tests assume UTC, not localtime"
- "Config loader panics if missing env var"
- "Pattern X is used everywhere; follow it"

These feed future Implementer Ralph runs.

---

### 3) Reviewer Ralph (Critic Ralph)

This persona is extremely powerful in a Ralph architecture.

**What it loads**

- Bundle diff
- Related Work
- Acceptance criteria
- Past Learnings in same module

**What it ignores**

- Backlog
- Planning debates
- Phase sequencing

**What it produces**

- Review findings
- Risk flags
- Potential new Learnings

Reviewer Ralph is not trying to build -- only to challenge assumptions.

Fresh context is essential here because:
It prevents confirmation bias from previous iteration narratives.

---

### 4) Spec/Design Ralph (Architect Ralph)

This one operates mostly at Plan/Spec/Phase levels.

**What it loads**

- Canonical Plan
- Current Spec
- Risk Learnings
- Past failed attempts (Iteration summaries)

**What it produces**

- Proposal records
- Clarifications
- Edge case expansions
- Spec Learnings

Spec learnings are often long-lived and highly reused.

Example:

- "Feature must degrade gracefully without Redis"
- "Mobile clients depend on idempotency"

These are gold.

---

### 5) Integrator Ralph (CI Brain)

This persona should be almost mechanical.

**What it loads**

- Accepted Bundles
- Current Tick SHA
- Integration rules
- Past failure Learnings

**What it produces**

- New Tick
- BundleStatus updates
- Failure Learnings

Example:

- "Running integration tests before unit tests reduces noise"
- "Order of migration scripts matters"

Integrator Ralph improves stability over time.

---

### The Key: Learnings Are Scoped

Not all personas load all learnings.

Each Learning record should include:

```
- scope_tags (paths, modules, levels)
- applicable_roles (optional filter)
- confidence
- created_at_tick
```

So when building a context:

```sql
SELECT learnings
WHERE scope intersects current scope
AND confidence >= threshold
AND applicable_roles contains this persona OR is null
ORDER BY confidence DESC, recency DESC
LIMIT N
```

That keeps context small and sharp.

---

### The Ralph Threading Model (Conceptual)

Think of it like this:

```
Iteration 1:
  Implementer discovers pattern mistake -> writes Learning A

Iteration 2:
  Reviewer loads Learning A -> catches related issue earlier

Iteration 3:
  Coordinator sees recurring class of issue -> writes Process Learning B

Iteration 4:
  Implementer loads B -> changes approach entirely
```

No chat memory.
Only structured cross-iteration memory.

---

### The Most Important Design Constraint

Never pass:

- previous prompts
- full chat transcripts
- long diff histories

Only pass:

- canonical state
- targeted learnings
- immediate work item

That is what makes the system scale cleanly.

---

### The Deep Insight

You are not building:

> A persistent conversation.

You are building:

> A distributed memory system that multiple stateless reasoning agents read from and write to.

That is why TaskStore fits so well.

---

### One Advanced Improvement

Add a "Learning decay" mechanism.

Each Learning has:

- reinforcement count (how often referenced)
- conflict count (how often contradicted)

Low reinforcement + old -> decays.
Frequently reinforced -> promoted to "Policy."

Policies are even more important and loaded more aggressively.

---

### Final Summary

Each persona runs Ralph independently:

- Fresh context every time.
- Loads only relevant canonical state.
- Loads scoped learnings.
- Produces structured artifacts.
- Writes durable memory.
- Dies.

The swarm effect emerges because:

- They share durable learnings.
- They operate on a shared tick-based reality.
- Coordinator and Integrator enforce coherence.

---

## You said:

This is all great. What is a Tick? is that just an iteration? are all of the ralph loops for each persona syncronized across the same tick/iteration or are they scoped/specific to each persona?

## ChatGPT said:

This is the right question.

If you don't get **Tick vs Iteration vs Ralph loop** straight, the system becomes conceptually muddy very fast.

Let's cleanly separate them.

---

### What Is a Tick?

A **Tick is a change in shared reality.**

Concretely:

> A Tick = a published advancement of the integration branch (a new canonical Git SHA) + recorded state transition in TaskStore.

It represents:

- A new repo truth
- A new baseline for all implementers
- A synchronization point

It is **not** an LLM iteration.

It is a **world-state advancement event**.

Think of it like:

- CI green -> deployable state
- "Main has advanced"
- A checkpoint in shared history

---

### What Is an Iteration?

An **Iteration is one Ralph execution of one persona.**

Example:

- Implementer Ralph runs once to produce a bundle
- Reviewer Ralph runs once to produce a review
- Coordinator Ralph runs once to reprioritize

Each of those is an iteration.

They:

- build fresh context
- read canonical state
- write structured output
- die

So:

| Concept | Meaning |
|---------|---------|
| Iteration | One stateless reasoning run |
| Tick | Advancement of shared world state |

Very different.

---

### Are All Personas Synchronized to the Same Tick?

Short answer:

> They all observe the same current Tick, but they do not run in lockstep.

More precise:

- Every iteration records which `base_tick` it observed.
- But iterations can happen at different times.
- Ticks only advance when the Integrator publishes.

So the system looks like this:

```
Tick 7 published (SHA abc123)

Implementer A runs iteration based on Tick 7
Reviewer B runs iteration based on Tick 7
Coordinator runs iteration based on Tick 7

Integrator merges bundle -> Tick 8 published (SHA def456)

Implementer A is now stale
Reviewer B might still review old bundle
Coordinator runs new iteration based on Tick 8
```

They share the same evolving world state, but are not synchronized cycles.

---

### Should Ralph Loops Be Globally Synchronized?

No.

Trying to synchronize all personas into one big "global iteration" creates:

- Idle time
- Bottlenecks
- Artificial coupling
- Reduced parallelism

Instead:

- Personas run independently.
- They react to Tick advancement.
- They replan when safe.

That's event-driven, not cycle-driven.

---

### So What Coordinates Everything?

Only two things are globally serialized:

1. **Coordinator decisions**
2. **Tick publication**

Everything else is concurrent.

This gives you:

- High parallel thought
- Controlled shared mutation
- Stable integration points

---

### Mental Model That Makes It Click

Think of it like a multiplayer game server:

- **Tick = world update**
- **Iteration = one player action**
- All players act independently.
- Server serializes world changes.
- After world changes, players update their local assumptions.

You don't synchronize all players before each action.
You synchronize the world state.

---

### Important Design Rule

Every iteration must record:

```
base_tick_number
```

And every Bundle must declare:

```
base_tick_sha
```

So you can answer:

- "Was this reasoning done on stale assumptions?"
- "Does this bundle need replay/rebase?"

Without that, your system becomes nondeterministic.

---

### How Plan/Spec/Phase Fit In

Now this is subtle.

Plan/Spec/Phase operate at higher levels than Code.

So you may have:

- Code Ticks (Git integration branch)
- Spec revisions (Decision records)
- Plan revisions

But only Code Ticks represent shared Git reality.

Spec and Plan changes are coordinated by the Coordinator, not the Integrator.

You don't need a Tick for spec changes unless they modify repo files.

---

### What Should Trigger a New Ralph Run?

Each persona should rerun when:

- Relevant Tick changes
- A dependent Decision changes
- A Work state changes
- A Lock changes
- A Bundle is submitted (for reviewers)
- A Bundle is merged/rejected (for implementers)

This is event-driven, not round-based.

---

### The Clean Model

Here's the clean separation:

```
TICK
  - Global shared world checkpoint
  - Only integrator advances
  - Serial

ITERATION
  - Stateless persona run
  - Many in parallel
  - Records base_tick

LEARNING
  - Durable cross-iteration memory
  - Scoped by tags + roles
  - Loaded per iteration
```

That's it.

---

### The One Thing You Must Avoid

Do not tie iteration count to tick count.

They will diverge massively.

Example:

- 50 Reviewer iterations
- 12 Implementer iterations
- 7 Coordinator iterations
- 3 Ticks published

That's healthy.

---

### Final Answer

- A **Tick is not an iteration.**
- A Tick is a serialized advancement of shared Git reality.
- Iterations are persona-specific Ralph loops.
- All personas observe the same current Tick.
- They are not globally synchronized.
- They react to Tick changes.

---

## You said:

any other detail in this system we should explore before I transfer this knowledge to another llm context? think about what you would want to know or would find extremely useful if you were receiving this information from another llm

## ChatGPT said:

Excellent question.

If I were about to inherit this system design in a fresh LLM context, there are **five categories of information** I would desperately want that we haven't fully pinned down yet.

These aren't implementation details.
They are **system-shaping constraints**.

---

### 1) What Is the Objective Function?

Right now we have architecture.

But what is the system optimizing for?

Speed?
Correctness?
Exploration breadth?
Determinism?
Minimal human oversight?
Low merge conflicts?
Spec fidelity?

These trade off against each other.

If I inherit this system without knowing the optimization priority, I will make wrong decisions.

Example differences:

- If optimizing for speed -> small bundles, aggressive ticks, more implementers.
- If optimizing for correctness -> slower ticks, stronger review gate, fewer implementers.
- If optimizing for determinism -> stricter base_tick enforcement.
- If optimizing for exploration -> parallel speculative bundles allowed.

You should write down:

> "This swarm is primarily optimized for ______."

That single sentence influences everything.

---

### 2) What Is the Trust Model?

This is huge.

Are agents assumed to:

- Be cooperative and aligned?
- Occasionally hallucinate?
- Frequently make structural mistakes?
- Attempt risky refactors?

If the trust model is:

> "Agents are fallible and must be verified"

Then Integrator must be very strict.

If:

> "Agents are mostly correct, small mistakes acceptable"

Then lighter validation is fine.

The architecture changes depending on this.

---

### 3) What Is the Conflict Philosophy?

When two bundles conflict:

Do you:

- Prefer automatic replay and adaptation?
- Prefer rejection and replan?
- Allow integrator to rewrite patches?
- Defer to Coordinator arbitration?

Different philosophies create very different system personalities.

Right now your system leans toward:

> Replan over automatic reconciliation.

That's important to document explicitly.

---

### 4) How Do Learnings Evolve?

We discussed learnings, but not:

- Who can invalidate them?
- Can learnings contradict each other?
- Do learnings require review before becoming high-confidence?
- Can they be promoted to policy?

If I inherit this system, I need to know:

Are learnings soft hints?
Or enforced constraints?

Because that changes how I build context selection.

---

### 5) What Is the Scope of Swarming?

You described Plan -> Spec -> Phase -> Code.

But are these sequential?
Or can multiple levels swarm simultaneously?

For example:

- Can Plan swarm while Code swarm is running?
- Can Spec revision interrupt Code execution?
- Does a Plan change invalidate all Works?

You need one clear rule:

> Does level advancement cause global reset of lower-level work?

That's architectural.

---

### 6) What Is the Failure Recovery Model?

When a Tick fails:

- Do we roll back?
- Do we mark bundles invalid?
- Do we freeze all implementers?
- Do we degrade gracefully?

When the Coordinator makes a bad decision:

- Is there revision history?
- Can decisions be reversed?
- Do we create compensating decisions?

This is rarely discussed, but critical.

---

### 7) What Is the Granularity of Works?

This one matters more than people think.

Are Works:

- 30-minute microtasks?
- 2-hour tasks?
- Multi-day epics?

Swarming only works if tasks are small and bounded.

If Works are too large:

- Implementers thrash.
- Rebase pain increases.
- Learnings become stale.

If too small:

- Coordinator becomes bottleneck.

Define target task size.

---

### 8) What Are the Human Override Points?

Eventually a human will intervene.

Where?

- Only at Coordinator?
- Can human inject Decision?
- Can human freeze integration?
- Can human manually publish a Tick?

If this is meant to be a "controlled Gastown,"
the control surface should be explicit.

---

### 9) How Deterministic Must It Be?

If you replay the same state with the same seed:

- Should you get the same result?
- Or is stochastic exploration desired?

This affects:

- Prompt design
- Sampling temperature
- Whether learnings are deterministic filters or advisory hints

---

### 10) Observability

If I inherit this system, I need to know:

- Can I reconstruct why a Decision was made?
- Can I see which Learning influenced a Bundle?
- Can I see which Tick caused replan events?

If you don't build observability in, the swarm becomes opaque.

At minimum you want:

- Iteration summaries
- Bundle lineage
- Tick history
- Learning provenance

---

### 11) The Most Important Missing Clarification

Right now we have:

- Coordinator
- Integrator
- Implementers
- Reviewers
- Spec swarm

But we haven't defined:

> What is the "Definition of Done" at each level?

Plan Done means what?
Spec Done means what?
Phase Done means what?
Code Done means what?

Without clear done criteria, the Coordinator becomes arbitrary.

---

### If I Were the Next LLM...

Here is the compact briefing I would want handed to me:

1. Objective function (primary optimization goal)
2. Trust model (how strict validation is)
3. Conflict philosophy (replan vs auto-merge)
4. Work granularity target
5. Level reset policy (Plan/Spec/Phase interactions)
6. Learning lifecycle rules
7. Human override surface
8. Determinism expectations

Everything else we've discussed is mechanical.

These 8 shape the personality of the entire swarm.

---

### My Personal Recommendation

Before transferring this to another LLM context, write a single document titled:

> Swarm System Operating Principles

No architecture diagrams.
No code.

Just constraints and philosophy.

That document will prevent architectural drift more than anything else.

---

## You said:

1. Correctness & Speed (in that order)
2. Agents are fallible and must be verified (a good validation/verification check will ensure the that ralph wiggum loop repeats until it passes this check)
3. these strike me as Strategies. could we not have some options to enable different behaviors? (if not, my plan was communicate a replan as soon as a new version landed in the repo; agents receiving this message do so at their nearest/safest convenience)
4. I am not sure of the answer to 1-3. but I do like the idea the learnings can be promoted to policy
5. they build upon each other and are hierarchical. meaning you cant begin work Code until you have phase.md (or whatever, maybe jsonl record); so yes you can Plan and Spec and Phase while working on the other things, but you cant build a spec.md or jsonl record until you have to plan to derive it from.
6. I think we roll back. yes there is a revision history; not sure on the other two. this is important i am just not sure at this point.
7. I think the work items are small; something to fit within a HALF of the context window; because that is the most usable portion of the context window, it hallucinates after that
8. I think normally the human interacts primarily in generating the plan; a conversation such as this for example. however, things will go wrong it would be nice to fix a record manually and ask the system to start over from a certain point. depending on implementation this may be easier or harder.
9. i dont know how to answer this question; these are Ralph Wiggum loops using an llm context. I think there is some inherent indeterminism
10. yes for the 3 questions you asked; reconstruct decision, learning influenced bundle, Tick caused replan; also copious logs, probably separated by type and project etc. something that makes it easy to debug this thing and share with an llm
11. we need to create bulletproof specs for all of these. that is part of the plan in my mind.

I answered your 11. items and I think that covered the 8 item list as well. let me know if I missed anything

## ChatGPT said:

This covers almost everything I'd want. The only true "missing pieces" are a few crisp **operating rules** you can hand to another LLM without ambiguity. I'll distill what you said into a tight spec, then list what's still undefined.

### Your operating principles distilled

#### Objective

- **Correctness first, speed second.**
- "Verified" means the loop repeats until it passes validation.

#### Trust / verification

- Agents are fallible.
- **No change is "real" until verified** (Integrator gate + checks).

#### Concurrency strategy (pluggable)

- You want **strategies** you can swap:
  - default behavior: *notify on new repo version; replan at nearest safe point*
  - potentially other behaviors (auto-replay, reject-stale, etc.)

#### Learnings lifecycle

- Learnings exist.
- Learnings can be **promoted to Policy**.

#### Hierarchical levels

- Plan -> Spec -> Phase -> Code are **strict dependencies**.
- You can work ahead (Plan/Spec/Phase) while other work proceeds, but you can't derive a lower level without the parent.

#### Failure handling

- Tick failure: **roll back**.
- Revision history exists.
- (TBD) what happens to in-flight bundles/work items and whether there are compensating actions.

#### Work item size

- Very small: **should fit in ~half of the usable context window**.

#### Human override

- Human mostly helps at Plan time.
- Human should be able to **edit a record manually** and restart from a chosen point.

#### Determinism

- Accept inherent LLM nondeterminism.

#### Observability

- Yes: reconstruct decisions, learning->bundle influence, tick->replan causality.
- Copious structured logs, separated by type/project; usable for debugging and feeding another LLM.

#### Definition of done

- You intend "bulletproof specs" for each level.

That's already a strong "worldview doc."

---

### The few things still missing (worth deciding or at least naming)

#### A) What exactly is "verification" at each level?

You said "repeat until it passes the check"--perfect. But the other LLM will need to know:

- **Code verification**: tests/lint/build? required commands? what's the "green bar"?
- **Phase verification**: is it a checklist satisfied? a dependency graph consistent? risk review done?
- **Spec verification**: acceptance criteria complete? ambiguity scan? test matrix present?
- **Plan verification**: success metrics + non-goals + constraints?

You don't need the full bulletproof spec now, but you *do* want the shape:

> Verification is a function `verify(level, artifact_set) -> pass/fail + reasons`.

#### B) Tick rollback semantics

"Roll back" is clear, but rollback can mean two things:

1. **Git rollback**: integration branch reset to previous SHA (hard reset / revert commit)
2. **TaskStore rollback**: tick record marked failed, no new published tick; bundles remain accepted but unmerged

You probably want:

- Published ticks are immutable "world checkpoints"
- Failed ticks never publish
- So "rollback" is mostly "don't publish; keep current tick"

If you *do* publish then revert, you'll need to model "tick 10 published then undone," which complicates staleness and provenance.

#### C) Manual repair / restart model

You said: "fix a record manually and start over from a certain point."

Two clean approaches:

- **Snapshot restart**: choose a tick + a set of records as baseline; truncate later history (rare)
- **Compensating record**: never delete history; write a "correction decision" that supersedes prior facts (common)

You don't have to choose now, but tell the next LLM which direction you prefer.

#### D) Strategy plug-ins: what are the knobs?

You asked about options/behaviors. A nice minimal set of strategy modes:

- **Stale policy**
  - `REPLAN_AT_SAFE_POINT` (your default)
  - `AUTO_REPLAY_AND_VERIFY` (integrator tries to replay stale bundles)
  - `REJECT_IF_STALE` (forces implementer to refresh)

- **Conflict policy**
  - `LOCK_STRICT` vs `LOCK_ADVISORY`

- **Tick cadence**
  - `BATCHED` vs `CONTINUOUS`

- **Bundle size policy**
  - max diff size / max files touched

Just naming these helps the next LLM reason about tradeoffs.

#### E) The "policy promotion" gate

If learnings can become policy, define:

- Who can promote? (Coordinator only is simplest.)
- What evidence is required? (e.g., reinforced N times, no contradictions, passed review.)

---

### Did you miss anything from my earlier list?

Only one: **conflict philosophy**. But you answered it implicitly with "notify + replan at safe point" and desire for strategy knobs. So you didn't miss it--you just want it configurable.

---

### A pasteable handoff summary for a new LLM context

If you want something you can paste verbatim, here's a compact version:

- Goal: correctness first, speed second. Agents are fallible and must be verified; loops repeat until verification passes.
- Architecture: stateless Ralph iterations per persona; durable state in TaskStore; code truth advances via integration "Ticks" (published integration SHA checkpoints). Ticks are serialized by Integrator; Coordinator is the single authority for assignments/decisions/locks.
- Concurrency: many "think" personas run in parallel; few implementers write repo in isolated worktrees; Integrator merges bundles and runs validations.
- Default stale handling: when a new tick lands, agents are notified; they replan at nearest safe point (not immediate rebase). Strategy knobs may allow auto-replay/reject-stale.
- Levels are hierarchical: Plan -> Spec -> Phase -> Code. You can work ahead, but cannot derive a lower level without its parent artifact.
- Learnings: structured, scoped; can be promoted to Policy (likely by Coordinator) based on reinforcement.
- Failure handling: tick failures should roll back (prefer "no publish" semantics); revision history exists.
- Works must be small (fit in ~half of usable LLM context).
- Human control: primarily at Plan creation; must be able to manually edit records and restart from a chosen point.
- Observability: must reconstruct why decisions were made, which learnings influenced bundles, which tick caused replans; strong structured logging.

---

## You said:

A) for Code, this is ci (lint, type check, test, using binary, playwright the website, etc); for docs Plan, Spec and Phase, I think we use separate llm as validator of the produced doc to some standard. This is why those docs need to be bulletproof
B) I agree with you for what I probably want
- Published ticks are immutable "world checkpoints"
- Failed ticks never publish
- So "rollback" is mostly "don't publish; keep current tick"
C) I like both approaches. I want a flag here with defaulting to the first one. snapshot delete history.
D) lets go with your list and any others you would suggest
E) Who can promote? (Coordinator only is simplest.)
> What evidence is required? (e.g., reinforced N times, no contradictions, passed review.)
you choose, make those values be options/knobs that we can adjust if need be.

## ChatGPT said:

Perfect -- with A-E answered, you now have a pretty complete "operating principles" spec. Below is (1) the tightened model incorporating your choices, (2) the strategy knobs (your list + a few I'd add), and (3) concrete defaults for policy promotion + doc validation that are adjustable.

### A) Verification model by level

#### Code verification

**Source of truth:** CI pipeline.

- Required checks (configurable): `lint`, `fmt`, `typecheck`, `unit`, `integration`, "use the binary", `playwright/e2e`, etc.
- **Tick publish rule:** integrator publishes only if CI required checks pass.

#### Plan / Spec / Phase verification

**Source of truth:** an LLM validator (separate persona) that evaluates the doc against a bulletproof rubric.

- Output is structured: `pass/fail`, list of violations, and optionally suggested fixes.
- The Coordinator treats a doc as canonical only when validator passes.

**Knob:** `validator_strictness` (see below).

---

### B) Tick semantics

Your chosen model (and I agree):

- **Published ticks are immutable world checkpoints.**
- **Failed ticks never publish.**
- "Rollback" = *don't advance tick; keep current tick* (but record failure artifacts + reasons).

This keeps causality and staleness clean.

---

### C) Manual repair / restart model

You want both, with a flag and default to snapshot.

#### Mode 1: Snapshot restart (default)

- "Reset world to Tick N + TaskStore snapshot N"
- Delete later history (or archive it elsewhere) so the system continues from a clean branch of reality.

**Default:** `restart_mode = SNAPSHOT_TRUNCATE`

#### Mode 2: Compensating correction

- Never delete history.
- Write a `CorrectionDecision` / `PolicyOverride` that supersedes earlier records.

**Knob:** `restart_mode = COMPENSATE`

(Implementation-wise, you can support both without huge complexity if you treat "snapshot" as creating a new root and optionally pruning.)

---

### D) Strategy knobs (your list + a few worth adding)

#### 1) Stale policy (when tick advances)

- `REPLAN_AT_SAFE_POINT` (default)
- `AUTO_REPLAY_AND_VERIFY` (integrator tries to replay stale bundles onto new tick + rerun CI subset/full)
- `REJECT_IF_STALE` (forces implementer to refresh before resubmitting)

#### 2) Conflict policy

- `LOCK_STRICT` (default): integration refuses bundles that touch locked resources not owned
- `LOCK_ADVISORY`: allow but warn; coordinator decides

#### 3) Tick cadence

- `BATCHED` (default): publish ticks on cadence or when queue reaches threshold
- `CONTINUOUS`: publish as soon as validated

#### 4) Bundle size policy

- `max_files_touched` (default: 8)
- `max_loc_changed` (default: 300)
- `max_time_in_progress_minutes` (default: 30-60 before forced replan)

#### 5) Validation depth (code)

- `ci_required_profile`:
  - `FAST` (lint + typecheck + unit)
  - `STANDARD` (FAST + integration)
  - `FULL` (STANDARD + e2e/playwright)
  - **Default:** `STANDARD`, with `FULL` on release-like ticks.

#### 6) Work allocation / swarming style (useful knob)

- `swarm_mode`:
  - `SPEC_HEAVY` (default for correctness): more proposal/review tasks, fewer implementers
  - `CODE_HEAVY` (speed): more implementers, more integration churn
  - `BALANCED`

#### 7) Isolation policy for implementers (recommended)

- `worktree_cleanliness`:
  - `HARD`: fresh worktree per work item (default)
  - `SOFT`: reuse worktree (faster, riskier)

#### 8) Doc validator strictness (Plan/Spec/Phase)

- `validator_strictness`:
  - `HARD_FAIL_ON_ANY_AMBIGUITY` (default)
  - `ALLOW_AMBIGUITY_WITH_FLAGS`
  - `SUGGEST_ONLY`

---

### E) Learnings -> Policy promotion rules (Coordinator-only) with adjustable knobs

#### Record types

- `Learning` (scoped, confidence, provenance)
- `Policy` (enforced constraint loaded aggressively)
- `PromotionReview` (optional evidence trail)

#### Default promotion algorithm

Coordinator can promote a Learning to Policy if:

1. **Reinforcement threshold met**
   - `reinforcement_count >= 3`
     - increment reinforcement when:
       - referenced by an accepted bundle, or
       - cited in a passing validator report, or
       - resolves a repeated failure reason.

2. **No unresolved contradictions**
   - `contradiction_count == 0`
     - contradiction when a later high-confidence learning conflicts, or validator flags inconsistency.

3. **Passed a quick "policy review" validator**
   - LLM validator checks: "Is this precise, non-ambiguous, actionable, and not overfitted?"
   - If pass -> promote.

#### Knobs to tune later

- `promotion_min_reinforcements` (default 3)
- `promotion_max_age_days` (default 30; older needs re-validation)
- `promotion_requires_validator` (default true)
- `promotion_contradiction_policy`:
  - `BLOCK` (default)
  - `ALLOW_WITH_PRIORITY` (policies have ranks)

---

### Pasteable updated handoff summary (tight + unambiguous)

- Objective: correctness first, speed second. Agents are fallible; loops repeat until verification passes.
- Roles: Coordinator (single authority for decisions/assignments/locks/policy promotion) + Integrator (single authority for publishing ticks) + bounded implementers (repo writers via worktrees) + parallel contributors (spec/review/research via TaskStore artifacts).
- Iteration vs Tick: Iteration = one persona Ralph run (fresh context). Tick = immutable published integration checkpoint (Git SHA). Failed ticks never publish.
- Verification: Code verified by CI (lint/typecheck/tests/e2e etc). Plan/Spec/Phase verified by separate LLM validator against bulletproof rubrics.
- Staleness: tick advance triggers notifications; default action is REPLAN_AT_SAFE_POINT. Other modes: AUTO_REPLAY_AND_VERIFY, REJECT_IF_STALE.
- Manual repair: restart_mode flag, default SNAPSHOT_TRUNCATE (reset to tick N, delete later history), optional COMPENSATE (append correction records).
- Learnings: structured/scoped; Coordinator can promote to Policy. Default: promote after 3 reinforcements, no contradictions, passes policy-validator. All thresholds are knobs.
- Observability: must reconstruct decisions, learning->bundle influence, tick->replan causality; structured logs by type/project.

---

If you want one more "high value" thing to carry into a new context: define the **doc validator rubrics** as checklists (Plan/Spec/Phase). Those rubrics will become your de facto product requirements and keep the LLM validator from drifting.
