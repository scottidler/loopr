# Roles and States

**Author:** Scott A. Idler
**Date:** 2026-04-20
**Status:** Reference (canonical, not a design doc)

This doc is the single source of truth for what each Role in v5 is, what
code actually performs it, and which FSM edges it is allowed to take.
It is a reference, not a proposal: every decision here is locked by
[`docs/vision.md`](vision.md) and the domain-crate FSM tables. If you
find a contradiction between this doc and either of those sources, the
source wins; fix this doc.

## Summary

v5 has **seven named Roles** and **two FSMs** that reference them. Roles
are not all the same kind of thing: some are LLM agents running Ralph
loops, some are deterministic Rust code, one is the daemon itself, one
is a pure function. Lumping them together as "agents" is a v4 habit
that confuses architectural discussion; this doc untangles it.

Three properties define what a Role actually is:

1. **Kind.** LLM Ralph loop, deterministic code, daemon-internal,
   or pure function.
2. **Owner.** The crate that holds the code performing the Role.
3. **FSM authority.** Which transition edges the Role appears on in
   `PlanStatus` and `WorkStatus`.

The canonical decision-resolving exchange around the `Reviewer` role
lives in the "How Reviews Flow" section below, so it never has to be
re-litigated.

## The Seven Roles

Defined in `crates/domain/src/role.rs`:

```rust
pub enum Role {
    Coordinator,
    Integrator,
    Implementer,
    Reviewer,
    Researcher,
    Decomposer,
    Director,
}
```

Grouped by what actually performs them at runtime:

| Role        | Kind                      | Owner crate     | In First Gate? |
|-------------|---------------------------|-----------------|----------------|
| Coordinator | Daemon-internal routing   | `loopr`         | Yes            |
| Integrator  | Deterministic Rust code   | `integrator`    | Yes            |
| Decomposer  | Function (LLM-backed)     | `decomposer`    | Yes            |
| Implementer | LLM Ralph loop            | `agents`        | Yes            |
| Reviewer    | LLM Ralph loop            | `agents`        | Yes            |
| Researcher  | LLM Ralph loop            | `agents`        | No (deferred)  |
| Director    | LLM Ralph loop            | `agents`        | No (deferred)  |

"First Gate" tracks [vision.md §First Gate](vision.md#first-gate) and
[§Explicitly Not in First Gate](vision.md#explicitly-not-in-first-gate).
Roles marked "No" exist as `Role` variants today, are referenced by FSM
edges, but have no running code yet.

### Coordinator

The **daemon itself**. Not an LLM agent, not a sub-agent, not a Ralph
loop. When `loopr` is running as a daemon, the reactive event loop in
`crates/loopr` holds the `Coordinator` role and uses it to authorize
the deterministic, forward-moving edges in both FSMs.

- Examples of Coordinator-authorized moves:
  - All dependencies satisfied → `Work: Pending -> Ready`
  - Implementer returned a Bundle → `Work: InProgress -> InReview`
  - Reviewer returned `Verdict::Reject` → `Work: InReview -> InProgress`
  - No active sessions remain on an Integrated Work → `Work: Integrated -> Done`
  - All child Works Done → `Plan: Active -> Complete` *(shared with Decomposer)*
  - A parent Plan goes Superseded → cascade child Works to `Superseded`
- Why it shows up as a `Role` variant at all: `#[derive(Fsm)]`
  requires every edge to be authorized by a `Role`. Giving the daemon
  its own Role lets the deterministic routing layer fire transitions
  through the same type-checked API as any LLM-backed actor, instead
  of a "FSM bypass" escape hatch.
- It is **never** an LLM agent. If you catch yourself reasoning about
  "what the Coordinator would decide," you are asking the wrong
  question. Coordinator decisions are mechanical consequences of
  TaskStore state, not an LLM's opinion.

### Integrator

A **deterministic Rust crate** (`crates/integrator`). Not an LLM agent.
Its Cargo manifest mechanically forbids an `llm` dependency.
[vision.md §integrator](vision.md#integrator) makes this an
architectural property, not a convention.

- What it does: takes accepted Bundles, merges onto the per-Plan
  integration branch, runs validation, and on success publishes a
  `Tick` record.
- FSM edges it authorizes:
  - `Work: InReview -> Integrated` (Reviewer approved, Bundle merged)
- `Integrated -> Done` is **not** an Integrator edge. Session state
  lives in `agents` + the daemon loop, so the "no active sessions"
  guard on that edge is a Coordinator-only concern; Integrator has
  no visibility into it.
- Same code, same inputs, same base commit → same Tick SHA or the
  same typed `IntegrationError`. No LLM opinions involved.

### Decomposer

A **pure function** that happens to call the LLM. Not a Ralph loop,
not an agent. Defined in `crates/decomposer` with two entry points:

```rust
fn plan(goal: &Goal, ctx: &mut Context) -> Result<Plan>;
fn decompose(plan: &Plan, ctx: &mut Context) -> Result<WorkDag>;
```

- FSM edge it authorizes:
  - `Plan: Active -> Complete` *(shared with Coordinator)*. The
    decomposer emits the terminal transition when its output matches
    an "all-deps-satisfied-and-done" shape; the Coordinator fires the
    same edge when runtime state reaches the same condition.
- Why it is a Role at all: same reason as Coordinator. The FSM needs
  an authorizing Role, and "the decomposer function" is a clearer
  authority tag than reusing Coordinator for a transition that is
  semantically the decomposer's output.
- v3 and v5 both explicitly choose "decomposer is a function, not an
  agent" (v4 briefly treated it as one; that added nothing). A future
  `Strategy` trait pluggable by config is the escape valve, not an
  agent loop.

### Implementer

An **LLM Ralph loop** in `crates/agents`:

```rust
fn run_implementer(work: &Work, deps: &Deps<...>) -> Result<Bundle>;
```

- Writes code in a sibling worktree, iterates until it produces a
  Bundle it is willing to submit, or hits a retry-strategy exhaustion.
- FSM edges it authorizes:
  - `Work: InProgress -> Blocked` *(shared with Coordinator)*: "I am
    stuck and need intervention"
  - `Work: InProgress -> InReview`: "I am done; please review"
- The Implementer does **not** return itself to `Ready`
  mid-flight. If stuck, it goes `Blocked` (with a reason, once Stage 7
  adds `blocked_reason`); only the `Coordinator` can reset via the
  override table. This is deliberate: `Blocked` with a reason is more
  informative than silently re-queuing.

### Reviewer

An **LLM Ralph loop** in `crates/agents`:

```rust
fn run_reviewer(bundle: &Bundle, deps: &Deps<...>) -> Result<Verdict>;
```

- Inspects a Bundle and returns a typed `Verdict` (approve / reject,
  with rationale). It is a **pure inspector**.
- FSM edges it authorizes: **none.** The Reviewer does not
  transition Work state directly.
- See "How Reviews Flow" below for why this role is structured this
  way and why an `Approved` state was rejected.

### Researcher

An **LLM Ralph loop** in `crates/agents`:

```rust
fn run_researcher(query: &Query, deps: &Deps<...>) -> Result<Finding>;
```

- Answers a typed question about the target repo (usually: "does a
  tool exist for X," or "which module already handles Y"); returns a
  `Finding` record that a caller can attach to a Plan, Spec, or Work.
- FSM edges it authorizes: **none.** Researcher emits artifacts, not
  state changes.
- Deferred from First Gate. No current run requires it; a stuck
  Implementer can escalate to the advisor retry strategy instead.

### Director

An **LLM Ralph loop** in `crates/agents`:

```rust
fn run_director(event: &Event, deps: &Deps<...>) -> Result<Action>;
```

- Handles escalations the Coordinator is not authorized to make:
  kicking an `Active` Plan back to `Draft` for re-decomposition,
  `Abandoning` a whole Plan, or `Superseding` a Work that has been
  overtaken.
- FSM edges it authorizes:
  - `Plan: Active -> Draft` *(override)*: re-decompose
  - `Plan: Pending -> Draft` *(override)*: re-interview
  - `Plan: {Active, Pending, Draft} -> {Superseded, Abandoned}`
    *(shared with Coordinator, but Director is the only Role authorized
    for these from `Active`/`Pending`/`Draft` without the override
    table). Coordinator drives them in the routine-cascade case, Director
    in the escalation case.
  - `Work: any-non-terminal -> {Superseded, Abandoned}`
    *(shared with Coordinator; same split of routine vs. escalation)*
- Deferred from First Gate. Until the Director agent is implemented
  in `agents`, escalations in First Gate runs result in exit-with-error;
  see [vision.md §Explicitly Not in First
  Gate](vision.md#explicitly-not-in-first-gate). The FSM edges are
  in the table so Stage 7's code can be written against them without
  another domain-crate change when the agent lands.

## The Two FSMs

Only `PlanStatus` has shipped. `WorkStatus` is designed in
[`crates/domain/docs/design/2026-04-20-hierarchy.md`](../crates/domain/docs/design/2026-04-20-hierarchy.md)
and is about to land in Stage 6. `Spec`, `Phase`, `Tick`, and `Bundle`
are deferred and will gain their own status enums when their records
are introduced.

### `PlanStatus` (shipped, v0.5.11-v0.5.16)

Six variants, terminal set `{Complete, Superseded, Abandoned}`.
Canonical source: `crates/domain/src/plan.rs`.

**Transitions:**

| From    | To         | By                       |
|---------|------------|--------------------------|
| Draft   | Pending    | Coordinator              |
| Draft   | Active     | Coordinator              |
| Draft   | Superseded | Coordinator, Director    |
| Draft   | Abandoned  | Coordinator, Director    |
| Pending | Active     | Coordinator              |
| Pending | Superseded | Coordinator, Director    |
| Pending | Abandoned  | Coordinator, Director    |
| Active  | Complete   | Coordinator, Decomposer  |
| Active  | Superseded | Coordinator, Director    |
| Active  | Abandoned  | Coordinator, Director    |

**Overrides:**

| From    | To    | By        |
|---------|-------|-----------|
| Active  | Draft | Director  |
| Pending | Draft | Director  |

### `WorkStatus` (Stage 6 draft)

Ten variants, terminal set `{Done, Superseded, Abandoned}`. Canonical
source once Stage 6 ships: `crates/domain/src/work.rs`; table locked
in `hierarchy.md`.

**Transitions** (see `hierarchy.md` for full per-edge rationale):

| From       | To         | By                       |
|------------|------------|--------------------------|
| Draft      | Pending    | Coordinator              |
| Draft      | Ready      | Coordinator              |
| Draft      | Superseded | Coordinator, Director    |
| Draft      | Abandoned  | Coordinator, Director    |
| Pending    | Ready      | Coordinator              |
| Pending    | Superseded | Coordinator, Director    |
| Pending    | Abandoned  | Coordinator, Director    |
| Ready      | InProgress | Coordinator              |
| Ready      | Blocked    | Coordinator              |
| Ready      | Superseded | Coordinator, Director    |
| Ready      | Abandoned  | Coordinator, Director    |
| InProgress | Blocked    | Coordinator, Implementer |
| InProgress | InReview   | Implementer              |
| InProgress | Superseded | Coordinator, Director    |
| InProgress | Abandoned  | Coordinator, Director    |
| Blocked    | Ready      | Coordinator              |
| Blocked    | Superseded | Coordinator, Director    |
| Blocked    | Abandoned  | Coordinator, Director    |
| InReview   | InProgress | Coordinator              |
| InReview   | Integrated | Integrator               |
| InReview   | Superseded | Coordinator, Director    |
| InReview   | Abandoned  | Coordinator, Director    |
| Integrated | Done       | Coordinator              |
| Integrated | Superseded | Coordinator, Director    |
| Integrated | Abandoned  | Coordinator, Director    |

**Overrides:**

| From       | To       | By          |
|------------|----------|-------------|
| Ready      | Done     | Coordinator |
| InProgress | Ready    | Coordinator |
| InProgress | InReview | Coordinator |
| InReview   | Ready    | Coordinator |

Notable edges explained:

- `Ready -> Done` (Coordinator, *override only*): the "no-op Work"
  bypass. Exists for (a) Works whose AC was already satisfied by
  concurrent commits before they started; (b) test/recovery
  bootstrap. Lives in the overrides table rather than transitions,
  so callers must use `Work::override_status(Done, Coordinator)`
  explicitly; a stray `Work::transition(Done, Coordinator)` from
  `Ready` is a typed error. Structural enforcement of "no AC
  skipping on the normal path."
- `Integrated -> Superseded`: present so a cascade from a superseded
  parent `Plan` does not fail mid-integration with
  `FsmError::NoTransition`. Every non-terminal state has a path to
  every terminal state.
- `InProgress -> InReview by Implementer` is the **only** Implementer-
  authored transition; the "submit" action is the Implementer's single
  state-changing authority.

## How Reviews Flow

The review pipeline has three actors, not one. This is the design
point most likely to be re-proposed ("why doesn't Reviewer just
transition the Work?"), so the resolution lives here.

**Flow:**

1. Implementer produces a `Bundle` and fires
   `Work: InProgress -> InReview`.
2. Coordinator (daemon) notices the `InReview` state and kicks off
   `run_reviewer(bundle, ...)`.
3. Reviewer returns `Result<Verdict>`. The `Verdict` is a typed
   artifact persisted to the TaskStore; it is **not** an FSM edge.
4. Coordinator reads the `Verdict`:
   - `Verdict::Reject` → Coordinator fires
     `Work: InReview -> InProgress`.
   - `Verdict::Approve` → Coordinator hands the Bundle to the
     `integrator` crate.
5. Integrator performs the merge and, on success, fires
   `Work: InReview -> Integrated`.
6. Coordinator fires `Work: Integrated -> Done` once the
   post-integration guard (no active sessions) is satisfied.

**Why Reviewer has no FSM edges:**

- Separation of concerns. Reviewer is an LLM doing semantic analysis
  ("is this code good?"). Integrator is deterministic Rust doing a
  mechanical operation ("does this merge cleanly and pass validation?").
  These failure modes are different and so are the recovery paths.
- Artifacts over state. The `Verdict` is a persisted record with
  rationale; `Approved` as a state would carry no information beyond
  "run_reviewer returned Ok(Verdict::Approve)" and would be duplicated
  in the Verdict record anyway.
- Routing layer stays deterministic. Keeping the Coordinator as the
  sole router between LLM opinion and mechanical action means the
  state machine advances in one place, not two.

**Why no `Approved` state between `InReview` and `Integrated`:**

Considered and rejected. An `Approved` state would exist only for the
milliseconds between "run_reviewer returned Approve" and "Integrator
picked up the Bundle." It adds an 11th variant, an extra matrix of
FSM tests, and a micro-state that persists no information the Verdict
record does not already carry. If the "approved but not yet merged"
window grows big enough to need its own state (e.g. Stage 8's
merge-conflict-resolution queue), earn it then with a design doc that
actually needs it.

## Why Coordinator and Director are Both Roles

This question recurs because v3 had only `Coordinator` (no Director),
v4 added Director, and v5 inherited v4's 7-Role shape without a recent
re-justification. The short answer: they serve disjoint purposes.

- **Coordinator** is the deterministic routing layer. Every edge it
  authorizes is a mechanical consequence of TaskStore state plus agent
  outputs. No LLM judgment is ever involved in whether a Coordinator
  transition fires.
- **Director** is the escalation layer, and is an LLM agent. Its job
  only exists when routine progress has broken down: a Plan needs to
  be re-decomposed, a Work is irrecoverably stuck, an entire objective
  needs to be abandoned. These are judgments, not mechanical
  consequences.
- Several edges appear in both Role lists (e.g.
  `Plan: Active -> Abandoned by (Coordinator, Director)`). That is not
  redundancy: Coordinator fires the edge in the routine cascade case
  ("parent Plan marked Abandoned, mechanically cascade to child
  Works"); Director fires the same edge in the escalation case ("this
  Plan is not recoverable, abandon it"). The FSM grants them both
  because either actor can legitimately reach that state; the calling
  context determines which actor does so in practice.

The alternative, collapsing Director into Coordinator, was rejected
because it would force the daemon to hold LLM-agent identity for the
escalation edges. The current split keeps the daemon's routing logic
free of LLM opinions and confines LLM-driven state resets to a named,
observable actor.

Until the Director agent is implemented (deferred from First Gate),
Director-authored edges are unreachable through normal runtime paths;
escalations exit with an error instead. The edges remain in the FSM
so Stage 7+ code can be written against them without a domain-crate
change when the agent lands.

## Adding a Role or a State

Additions are cheap if they respect the existing categorization.
Guidelines for future stages:

- **A new Role** should have a clear `Kind` from the taxonomy above
  (LLM Ralph loop, deterministic code, daemon-internal, function). If
  it does not fit, the taxonomy needs revisiting before the Role is
  added. Do not invent a fifth Kind silently.
- **A new status variant** must have at least one "in" edge and,
  unless terminal, a path to every terminal state. This invariant is
  what keeps cascade transitions from dying mid-flight with
  `FsmError::NoTransition`.
- **A new FSM edge** must pick an authorizing Role from the seven.
  Do not add new Role variants just to author a new edge; prefer
  reusing an existing Role unless the new one represents a genuinely
  different actor kind.
- **A new record kind's FSM** follows the `PlanStatus` / `WorkStatus`
  pattern: `#[derive(Fsm)]` with `role = crate::Role`, a `terminal`
  set, a `transitions(...)` block, and an `overrides(...)` block.
  Document rationale in a per-record design doc under
  `crates/domain/docs/design/`, not in this reference.

## See also

- [vision.md](vision.md): architectural shape; canonical for the
  distinction between LLM-calling and non-LLM crates.
- [`crates/domain/CLAUDE.md`](../crates/domain/CLAUDE.md): scope
  rules for the crate where the FSMs live.
- [`crates/domain/docs/design/2026-04-20-hierarchy.md`](../crates/domain/docs/design/2026-04-20-hierarchy.md):
  `WorkStatus` design, the source for the 10-state table above.
- [`crates/domain/docs/design/2026-04-20-fsm-macro.md`](../crates/domain/docs/design/2026-04-20-fsm-macro.md):
  `#[derive(Fsm)]` contract.
- [`crates/domain/src/role.rs`](../crates/domain/src/role.rs):
  canonical `Role` enum.
- [`crates/domain/src/plan.rs`](../crates/domain/src/plan.rs):
  shipped `PlanStatus` FSM.
