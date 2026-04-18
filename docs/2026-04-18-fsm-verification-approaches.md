# FSM Verification Approaches for loopr-v4

Status: Proposal
Author: Scott Idler (with Claude)
Date: 2026-04-18

## Context

loopr-v4 runs on five interacting finite-state machines:

| FSM       | States | Source                          |
|-----------|--------|---------------------------------|
| Work      | 10     | `src/domain/work.rs:30`         |
| Bundle    | 8      | `src/domain/bundle.rs:11`       |
| Hierarchy | 6      | `src/domain/plan.rs:18`         |
| Tick      | 5      | `src/domain/tick.rs:11`         |
| Agent     | 8      | `src/agents/status.rs:8`        |

Roughly 37 states and ~70 valid transitions (+ ~5 override edges), with role-based authorization on each edge. The spec lives as YAML under `resources/engine/fsm/`; the runtime is `FsmInterpreter` in `src/fsm/runtime.rs`.

Current test coverage (`src/tests/fsm/*`, ~2,525 lines) is exhaustive at the **per-edge matrix** level: every (from, to, role) triple is exercised. What it does not cover:

- Drift between the Rust enums and the YAML spec.
- Graph-shape properties (reachability, orphan terminals, dead ends).
- Multi-step sequences (paths / N-switch coverage).
- Cross-FSM invariants once guards land (Doc 4 deferred work).

This document describes four complementary verification approaches, in priority order, and what each one catches that the others miss.

---

## 1. Model ↔ Implementation Conformance

### What it is

Assert at test time that the Rust-side status enums (`WorkStatus`, `BundleStatus`, `HierarchyStatus`, `TickStatus`, `AgentStatus`) and the YAML-side FSM definitions stay in lockstep:

- Every variant returned by `FsmStatus::all_variants()` appears as a state in the YAML.
- Every state name in the YAML round-trips through `from_yaml_name()` → `to_yaml_name()`.
- Every state marked `terminal: true` in YAML has zero outgoing transitions (normal or override).
- Every `by:` role in YAML is a known role in `src/agents/role.rs`.

### Why loopr needs it

The YAML is the source of truth, but Rust code consumes it through the `FsmStatus` trait with hand-written variant mappings. Adding a state to the enum without updating the YAML (or vice versa) is silent today; the per-edge matrix tests iterate over the enum and will simply skip any YAML state that has no matching variant.

### Where it fits

Extend `src/tests/fsm/status.rs`. No new crates, no new infra. One-time cost ~1 hour.

### What it catches

- Orphan YAML states that no Rust variant references.
- Enum variants missing from the YAML.
- Terminal flag contradictions (a state flagged terminal but still an edge source).
- Typo'd role names in YAML guard lists.

---

## 2. Graph-Shape Properties (Reachability & Liveness)

### What it is

Treat each loaded `FsmDefinition` as a directed graph and assert structural properties by BFS/DFS:

- Every non-initial state is **reachable** from the initial state by *some* sequence of edges.
- Every non-terminal state has **at least one outgoing edge** (no dead ends).
- Every terminal state is **reachable** from the initial state (no orphan terminals).
- Optional: every state can reach *some* terminal (liveness — no infinite loops in the spec).

### Why loopr needs it

The YAML spec is hand-authored. It is easy to introduce a state that nothing transitions into, or a non-terminal state with no outgoing edges (a trap). Neither shows up in matrix tests because those tests only verify the edges that *do* exist — they don't ask "are there enough edges?"

As the FSMs evolve (new statuses for Bundle triage, new intermediate Work states), this class of bug will recur.

### Where it fits

New file `src/tests/fsm/graph.rs` with a small graph module inside it (adjacency map over the loaded `FsmDefinition`). Cost ~2 hours.

### What it catches

- Unreachable states (spec author added a state and forgot to wire it in).
- Dead-end non-terminals (spec author deleted the outgoing edge but left the state non-terminal).
- Orphan terminals (terminal state with no path in — probably a renamed state whose incoming edges still point elsewhere).
- Livelocks at the spec level (cycle with no escape to any terminal).

---

## 3. Property-Based Stateful Testing (Path Coverage)

### What it is

Random walks over the FSM, comparing the YAML spec (reference model) against `FsmInterpreter` (system under test). For every random sequence of (role, event) inputs of length N, assert:

- The reference and the implementation agree on accept/reject.
- If accepted, they agree on the resulting state.
- Invariants hold at every step (terminal states stay terminal, role authorization is never bypassed).

Implemented with `proptest` (already idiomatic in Rust) using either `proptest-state-machine` or a hand-rolled `ReferenceStateMachine`.

### Why loopr needs it

Current tests cover every edge **once**. They do not cover **sequences**. The bugs that hurt in role-gated workflows live in paths:

> A Coordinator pushed Work through Pending → Ready → InProgress. A Reviewer then tried InProgress → InReview, but between YAML versions the `by:` list changed and the test matrix was regenerated from the new spec, so the regression never surfaced.

This is exactly what Chow's W-method is designed to catch, except property-based testing gives you the same coverage with less hand-authored code. Shrinking also gives you minimal failing counterexamples for free — a 4-step path instead of a 50-step one.

This becomes **essential** once guard conditions (Doc 4) land, because the transition tuple grows from `(from, to, role)` to `(from, to, role, guard_result)` and the matrix approach no longer scales.

### Where it fits

`src/tests/fsm/proptest.rs` — one property-based test per FSM. The reference walker reads directly from the loaded `FsmDefinition`, so the tests don't drift when the YAML changes. Cost ~1 day for all five FSMs.

### What it catches

- Role-authorization holes that only surface after a specific prefix of transitions.
- Idempotency violations (`Transition::Unchanged` returning stale state).
- Path-dependent accept/reject mismatches between spec and implementation.
- Regressions introduced by YAML edits that pass every per-edge test.

---

## 4. Cross-FSM Model Checking

### What it is

Encode the **composite system** — Work + Bundle + Hierarchy + Tick (and optionally Agent) — as a single model, and exhaustively check invariants that span multiple FSMs. Candidate tools:

- **Stateright** (Rust-native, fits the existing toolchain, runs as a `cargo test`-ish harness).
- **TLA+ / TLC** (more expressive, steeper curve, out-of-tree).

Express invariants like:

- "A Work cannot be `Done` if its containing Bundle is `Rejected`."
- "If a Tick is `Published`, every Work it references is in a terminal state."
- "A Hierarchy cannot re-enter `Draft` unless the Director role performs the override."

### Why loopr might need it

loopr's FSMs are not independent — they are embedded in a hierarchy (Plan → Spec → Phase → Work) and sequenced by Ticks. Per-FSM tests cannot catch *cross-FSM* corruption. Whether that matters depends on whether bugs of that shape have actually hurt the project. If the answer is "not yet," defer this.

### Where it fits

A new workspace crate, `loopr-model/`, containing a Stateright model that mirrors the YAML definitions. Not part of the main build — run on demand or in a nightly CI job.

### What it catches

- Composite-state invariants that no single-FSM test can express.
- Interleaving bugs (Work transitions and Bundle transitions racing).
- Protocol violations in the Plan→Spec→Phase→Work hierarchy.

### When to skip it

If no bug of this shape has ever shipped, defer it. Stateright is a real investment; only spend it against a real threat. Approaches 1–3 catch most regressions at a fraction of the cost.

---

## Priority and Sequencing

| # | Approach                     | Cost       | Payoff   | Recommendation         |
|---|------------------------------|------------|----------|------------------------|
| 1 | Model ↔ Impl conformance     | ~1 hour    | High     | Do first               |
| 2 | Graph-shape properties       | ~2 hours   | High     | Do next                |
| 3 | Property-based stateful      | ~1 day     | Highest  | Do before guards land  |
| 4 | Cross-FSM model checking     | ~1 week    | Variable | Defer until needed     |

Approaches 1 and 2 are mechanical and ship in a single PR. Approach 3 is the strategic investment — it replaces the hand-written matrix tests as the FSMs grow and makes guard-condition testing tractable. Approach 4 is held in reserve for a specific class of bug that has not yet hurt the project.

## What Not To Do

- **TLA+ from scratch.** The YAML already *is* a formal model; rewriting it in TLA+ duplicates the spec without adding leverage.
- **Chow / Wp-method by hand.** Property-based stateful testing (Approach 3) gives the same coverage guarantee with shrinking, in less code.
- **Deleting matrix tests.** Approach 3 supplements them, not replaces them. Matrix tests are the canonical "here is the expected edge" documentation and should stay.

## Open Questions

- Should Approach 3's reference walker share code with `FsmInterpreter` (risk: tautology — both sides agree because they share logic), or be a fresh YAML reader (risk: drift between the two readers)? Lean toward a fresh reader for the reference.
- Should Approach 4 include `AgentStatus`, which intentionally omits role authorization? Probably yes, but with a separate invariant set.
- Which invariants are load-bearing for loopr's correctness? The list in Approach 4 is illustrative; the real list should come from past incidents and the v4 architecture doc.
