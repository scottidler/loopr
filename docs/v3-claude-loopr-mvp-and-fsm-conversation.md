# Loopr MVP Scoping, FSM Architecture & Design Planning

## User said:

read this docs/chatgpt-loopr-architecture-conversation.md

## Claude said:

*(After reading the full ~2700-line document)*

Here's a summary of the ChatGPT conversation:

### What Loopr Is

A **TUI-based "dev team in a box"** that implements the **Ralph Wiggum Loop** (each iteration = fresh LLM context) across four hierarchical levels: **Plan -> Spec -> Phase -> Code**. It's inspired by Beads/Gastown but aims for cleaner abstractions.

### Core Architecture

- **TaskStore** (`scottidler/taskstore` crate) is the persistence spine: JSONL as source of truth, SQLite as read cache, Git merge driver for collaboration
- **Tick** = immutable integration checkpoint (Git SHA). Published by Integrator only. Failed ticks never publish.
- **Iteration** = one stateless persona Ralph run. Many happen per tick. Not synchronized across personas.

### Roles (Personas)

1. **Coordinator** (single authority) -- PM/EM: assigns work, grants locks, makes decisions, promotes learnings to policy
2. **Integrator** (single authority) -- Release engineer: merges bundles, runs CI, publishes ticks
3. **Implementers** (bounded, 2-4) -- Senior engineers: own worktrees, produce atomic Bundles
4. **Spec/Design swarm** (high parallel, Tokio tasks) -- Architects: propose spec deltas, risks, test plans
5. **Reviewer swarm** (high parallel) -- Critics: review bundles, find issues
6. **Researchers** (high parallel) -- Librarians: search, summarize, find prior art

### Key Records (~7 types)

`Iteration`, `WorkItem`, `Lock`, `Proposal`, `Decision`, `Bundle`, `Tick` -- plus `Learning` (scoped, promotable to Policy)

### Operating Principles

- **Correctness first, speed second**
- Agents are fallible -> everything verified (CI for code, LLM validator for docs)
- Fresh context every iteration; only TaskStore remembers
- Staleness: notify on tick advance, replan at safe point (configurable strategy)
- Learnings are scoped, structured, promotable to Policy (3 reinforcements, no contradictions, passes validator)
- Human override: primarily at Plan; can snapshot-restart from any tick

### Strategy Knobs

Stale policy, conflict policy, tick cadence, bundle size, CI depth, swarm mode, worktree cleanliness, validator strictness, promotion thresholds, restart mode

---

## User said:

you are going to help me build a comprehensive design/plan to build this.

## Claude said:

Before diving into a design doc, I identified several orienting questions:

1. **Are you building this in Rust?**
2. **What's the current state of the repo?**
3. **Do you want to use the `/create-design-doc` skill?**
4. **Should we scope to an MVP first?**

---

## User said:

1. oh yeah baby. always in Rust
2. I have taken some whacks at this, but thus far nothing has stuck or been satisfactory
3. yes eventually. but I think we need to get some more peices on the table first
4. we should mvp. do the notes from the chatgpt convo mention any ideas?

## Claude said:

Yes! The ChatGPT conversation explicitly laid out an MVP order:

> **What you can implement first (MVP order)**
> 1. TaskStore records: WorkItem, Bundle, Tick, Lock, Iteration
> 2. Coordinator: create/assign/lock/accept bundle
> 3. Implementer process: worktree -> propose bundle
> 4. Integrator process: merge bundles -> run tests -> publish tick
> 5. Staleness marking + "replan required" gate before bundle submit
>
> Everything else (spec swarm, review swarm, plan/spec/phase levels) plugs into this backbone.

It also said the key insight is to build around a **"work item -> bundle -> tick" pipeline** -- if you get that right, everything else becomes manageable.

### Repo exploration findings:

**v1** -- completed a 10-phase implementation but was essentially a proof of concept. Architecture wasn't satisfactory.

**v2** -- a daemon+IPC+TUI rewrite that got pretty far (65+ source files, IPC protocol, daemon handlers, LLM client, tool system, etc.) but hit a wall. Last meaningful commit was "save point before trying v3."

**v3** -- empty orphan branch (current). Clean slate.

**TaskStore** -- is mature and ready. Clean API: `Store::open()`, `create/get/update/delete/list`, `Filter`/`FilterOp`, `Record` trait, JSONL source of truth, SQLite cache, merge driver, git hooks.

### Pieces needed on the table before the design doc:

1. **What to salvage from v1/v2** -- 45+ design docs and working code for LLM client, IPC, TUI, tool system, worktree management. Cherry-pick patterns, or truly start fresh?
2. **The "swarm" vs "single agent" question for MVP** -- multi-persona swarm, or single Coordinator+Implementer+Integrator?
3. **LLM integration approach** -- custom Anthropic client, Claude Code SDK, Agent SDK?
4. **Target repo model** -- operating on itself, a separate repo, or both?
5. **TUI scope for MVP** -- CLI-only first, minimal TUI, or TUI must-have?

---

## User said:

I asked the previous chatgpt thread this question:

how would you scope the MVP of this thing? Rust + ratatui

Here's how I'd scope an MVP that actually proves the system works, without building a spaceship. Rust + ratatui, with TaskStore as your state spine.

### MVP goal

Demonstrate a full end-to-end loop:

- Create Plan -> Spec -> Phase
- Create a small Code work item
- Implementer produces a Bundle in a worktree
- Integrator publishes a Tick after verification
- Everyone else marks stale + replans (at least in data)

If you can do that once, you have the core.

### MVP1: "Single-user swarm" (no real parallelism yet)

Keep it mostly serial to reduce complexity, but preserve the architecture.

#### Components

**TUI app (ratatui)**

Views:

- Dashboard (current tick, active plan/spec/phase, queue counts)
- Work Items (list + details + state transitions)
- Bundles (list + status)
- Ticks (history + current)
- Learnings/Policies (list + promote/demote)

Actions:

- create/edit records
- transition work item states
- accept/reject bundle
- publish tick (manual)

**TaskStore schema** -- Implement only these records:

- Plan, Spec, Phase (can just be markdown strings + status + parent IDs)
- WorkItem
- Bundle
- Tick
- Learning (optional in MVP1, but nice)
- Lock (optional; can stub as advisory)

**Worktree manager (minimal)**

- Create worktree per work item: `git worktree add .worktrees/<work_item_id> -b agent/<id>/<work_item_id> <base_sha>`
- "Bundle propose" command: collect touched paths, commit range, base tick sha; store as Bundle record

**Integrator (manual or semi-automatic)**

- For MVP1, do NOT build a full CI runner.
- Validation command can be a configured shell command list:
  - `cargo fmt --check`
  - `cargo clippy -- -D warnings`
  - `cargo test`
- Publish tick if commands pass.

#### Why this MVP is good

- Proves Tick vs Iteration separation.
- Proves worktree isolation.
- Proves Bundle -> Tick pipeline.

## Claude said:

That's a solid MVP scope. Key observations:

- **It's serial but architecturally honest** -- you're not faking the swarm, you're just running it with one actor at a time. The data model and state machines are real.
- **Human-as-Coordinator via TUI** -- you manually create plans/specs/phases, transition work items, accept/reject bundles, publish ticks. This lets you validate the whole pipeline before automating any persona.
- **Worktree isolation is real from day one** -- even in MVP, bundles come from actual worktrees.
- **Integrator is a shell command list** -- `cargo fmt --check && cargo clippy && cargo test`. Simple, honest, extensible later.

### How this answers the earlier orienting questions:

| Question | MVP1 Answer |
|----------|-------------|
| Salvage from v1/v2? | Architecture ideas yes, code probably not (v3 clean slate) |
| Swarm vs single agent? | Single-user swarm -- human is Coordinator, serial execution |
| LLM integration? | Not in MVP1 |
| Target repo model? | Separate target repo (worktrees prove the model) |
| TUI scope? | Yes, ratatui from day one -- it's the control surface |

---

## User said:

I reread that mvp from chatgpt; did it say "no LLM in MVP1"? I didnt see it. are you sure?

## Claude said:

You're right, it didn't say that. I inferred it because the MVP1 description focused on the TUI as the control surface with manual actions (create/edit records, transition states, accept/reject bundles, publish ticks manually). But the ChatGPT scope never explicitly excluded LLM -- it just didn't mention it either.

The real question is: **do you want the MVP1 Implementer to be LLM-powered, or human-driven?**

---

## User said:

is there an LLM involved in this MVP1 phase? you didnt mention it. can you be explicit?

*(Quoting ChatGPT's response):*

> Great catch.
>
> No -- in the MVP1 I described, there is no LLM involved at all.
>
> That was intentional.
>
> ### Why MVP1 Should Have No LLM
>
> Because your hardest problems are not LLM problems.
>
> They are:
>
> - Tick semantics
> - WorkItem -> Bundle -> Tick pipeline
> - Git worktree isolation
> - State transitions
> - Validation gating
> - TUI usability
> - Restart model
> - Observability
>
> If you introduce an LLM too early:
>
> - You'll debug prompt failures instead of architecture.
> - You won't know whether failures are orchestration bugs or hallucinations.
> - Your iteration semantics won't be trustworthy yet.
> - You'll hide architectural flaws behind LLM noise.
>
> ### Explicit MVP1 Definition (LLM-Free)
>
> MVP1 is: A Rust + ratatui application that orchestrates Plan/Spec/Phase/Code records, isolated git worktrees, bundle proposals, and integration ticks with CI validation -- with all decisions made manually by a human. No LLM. Zero.
>
> ### Where the First LLM Enters (MVP2)
>
> MVP2 should introduce exactly one LLM persona: **Doc Validator LLM for Spec/Phase/Plan gating.** Because it is read-only, produces structured validation reports, does not modify repo, does not mutate truth, and cannot break Tick semantics. It's the safest place to add intelligence.
>
> ### When Implementer LLM Enters
>
> Not until WorkItem -> Bundle -> Tick works flawlessly, staleness marking works, replan-at-safe-point works, CI gating is deterministic, and logs are reconstructable.
>
> ### The Big Principle
>
> You are building a distributed memory + coordination system. The LLM is a pluggable reasoning engine. Prove the system works first. Then insert brains.
>
> ### Summary
>
> - MVP1: No LLM
> - MVP2: LLM as doc validator
> - MVP3+: LLM implementers + reviewers

## Claude said:

The core argument is strong:

> **Your hardest problems are not LLM problems. They are orchestration problems.** Prove the system works mechanically, then insert brains.

The phasing:

- **MVP1**: No LLM. Human drives everything. Proves the spine.
- **MVP2**: LLM as doc validator (read-only, can't break anything).
- **MVP3+**: LLM implementers + reviewers.

---

## User said:

good enough for me for that part. topic change. there is a FiniteStateMachine or two in the heart of this thing. Should we be encoding/enforcing that with an FSM crate. The data is loaded from disk jsonl, sqlite3 and git, but when hydrated into code, should it be manifest inside the type-constricting constraints of an FSM?

## Claude said:

That's a sharp question. It sits at the intersection of your "correctness first" principle and Rust's type system strengths.

The tension is:

1. **On disk**: states are just strings/enums in JSONL -- flat, serializable
2. **In memory**: you want the type system to **prevent invalid transitions** at compile time (or at least enforce them rigorously at runtime)

### Rust FSM Ecosystem Research

| Crate | Maintained | API Style | Async | Serde | Guards | Downloads |
|-------|-----------|-----------|-------|-------|--------|-----------|
| **statig** 0.4 | Yes (2025) | Attribute macro + enum | Yes | Yes (feature) | Code in handlers | ~1.1K |
| **rust-fsm** 0.8 | Yes (2025) | DSL macro | No | No | No | ~650K |
| **sm** 0.9 | **No** (archived) | Typestate macro | No | No | No | - |
| **finny** 0.2 | Stale (2024) | Builder proc macro | No | No | Yes | ~low |
| **machine** | Unclear | Proc macro + enum | No | No | Conditional | ~low |
| **edfsm** | Yes (2025) | Attribute macro | Yes | No | Via code | ~low |
| **smlang** | Moderate | DSL macro | No | No | Yes | ~low |
| **Typestate pattern** | N/A | Hand-written generics | N/A | Painful | Code in methods | N/A |

### Why No Crate Fits

- **`statig`** is the strongest candidate but designed for reactive event-loop systems, not domain-model record stores. The `UninitializedStateMachine` deserialization dance and handler-method-per-state pattern make it awkward for "load JSONL record, validate transition, save."
- **`rust-fsm`** has no guards, no serde, no async. You'd build everything around it, making the crate pointless.
- Everything else is dead, stale, or too low adoption.

### Why Typestate Doesn't Work Here

- `WorkItem<Draft>` and `WorkItem<Ready>` are different types. Can't deserialize without knowing state at parse time.
- Can't have `Vec<WorkItem<_>>` with mixed states. Need a wrapper enum, which defeats the typestate benefit.
- 3 state machines x 6-8 states = massive boilerplate.

---

## User said:

*(Shared Kagi search screenshot confirming there's no de facto standard Rust FSM crate)*

## Claude said:

Kagi search confirms the research -- **there is no de facto standard**. The ecosystem is fragmented.

### Recommendation: Hand-rolled enum + transition table

No crate. Here's why it's the right fit for Loopr:

**The core problem:** Records live in JSONL (serde). When you deserialize a `WorkItem`, you don't know its state at compile time. Pure typestate doesn't work because you can't have `Vec<WorkItem<_>>` with mixed states, and serde round-tripping becomes painful.

**The pattern:**

```rust
// State is just a serde-friendly enum
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
pub enum WorkItemStatus {
    Draft, Ready, InProgress, Blocked, InReview, Integrated, Done, Abandoned,
}

// Transition table is a const array -- single source of truth, auditable
const WORK_ITEM_TRANSITIONS: &[(WorkItemStatus, WorkItemStatus, Option<Role>)] = &[
    (Draft,      Ready,      Some(Role::Coordinator)),
    (Ready,      InProgress, Some(Role::Coordinator)),
    (InProgress, Blocked,    None), // any role
    (InProgress, InReview,   Some(Role::Implementer)),
    (InReview,   InProgress, Some(Role::Coordinator)), // rejection
    (InReview,   Integrated, Some(Role::Integrator)),
    // ...
];

impl WorkItem {
    pub fn transition(&mut self, target: WorkItemStatus, role: Role) -> Result<()> {
        let allowed = WORK_ITEM_TRANSITIONS.iter().any(|(from, to, required)| {
            *from == self.status && *to == target
                && required.map_or(true, |r| r == role)
        });
        if !allowed {
            return Err(TransitionError::invalid(self.status, target, role));
        }
        self.validate_invariants_for(target)?;
        self.status = target;
        self.updated_at = now_ms();
        Ok(())
    }
}
```

**Why this wins:**

- **Serde is trivial** -- enum derives `Serialize`/`Deserialize`, JSONL round-trips perfectly
- **Transition table is data** -- auditable, testable, can generate Graphviz diagrams from it
- **Role guards are declarative** -- one line per transition, not scattered across handler methods
- **Per-state invariants** via `match` -- compiler warns if you add a state and forget its invariants
- **No macro magic** -- plain Rust, any developer can read it
- **Same pattern for all three** -- WorkItem, Bundle, Tick each get their own transition table
- **Tests are trivial** -- assert every valid transition succeeds, assert every invalid one fails

Can also write a test that exhaustively walks the transition table and generates a dot file for Graphviz visualization -- good for the observability requirement.

### State Machines from the ChatGPT Conversation

The ChatGPT conversation defined three explicit state machines with allowed transitions and role-based guards:

#### WorkItem States

Draft -> Ready -> InProgress -> Blocked -> InReview -> Integrated -> Done -> Abandoned

Transitions:

- Draft -> Ready (Coordinator)
- Ready -> InProgress (Coordinator assigns OR implementer claims with coordinator approval)
- InProgress -> Blocked (Implementer or Coordinator)
- Blocked -> Ready (Coordinator after resolving dependency/decision)
- InProgress -> InReview (Implementer submits Bundle)
- InReview -> InProgress (Coordinator requests changes; bundle rejected)
- InReview -> Integrated (Integrator merges accepted bundle)
- Integrated -> Done (Coordinator/Integrator marks verified)
- Any -> Abandoned (Coordinator only)

Invariants:

1. Single assignee in InProgress/InReview
2. Ready implies scope is bounded (acceptance_ref or checklist)
3. InReview implies exactly one "active" Bundle
4. Dependencies are acyclic
5. WorkItem must declare resource tags

#### Bundle States

Proposed -> Triaged -> Reviewed -> Accepted -> Integrating -> Merged -> Rejected -> Superseded

Transitions:

- Proposed -> Triaged (Coordinator)
- Triaged -> Reviewed (Coordinator triggers review swarm)
- Reviewed -> Accepted (Coordinator)
- Accepted -> Integrating (Integrator)
- Integrating -> Merged (Integrator)
- Integrating -> Rejected (Integrator)
- Proposed/Triaged/Reviewed -> Rejected (Coordinator)
- Any non-final -> Superseded (Coordinator when a newer bundle replaces it)

Required fields:

- `work_item_id`
- `base_tick_sha` (or base tick number)
- `touched_paths[]`
- `patch_ref` (branch+range or patch artifact)
- `claims` (what it asserts)
- `verification` (what was run)
- optional: `locks_used[]`

Invariants:

1. Bundle must declare its base
2. Integrator never merges a stale bundle blindly
3. At most one Accepted bundle per WorkItem at a time
4. Bundle cannot touch locked resources it doesn't own
5. Verification metadata is required

#### Tick States

Open -> Sealing -> Validating -> Published -> Failed

Transitions:

- Open -> Sealing (Integrator)
- Sealing -> Validating (Integrator)
- Validating -> Published (Integrator)
- Validating -> Failed (Integrator)
- Failed -> Open (Integrator starts a new tick)

Invariants:

1. Only one tick can be in Sealing/Validating at a time
2. Published tick has exactly one integration SHA
3. Tick records exactly which bundles it attempted
4. A tick failure must be attributable
5. Publishing a tick triggers staleness events

---

## Summary: What's On the Table for the Design Doc

### Architecture (from ChatGPT conversation)

- Full persona model (Coordinator, Integrator, Implementers, Spec/Review/Research swarms)
- TaskStore as persistence spine (JSONL truth, SQLite cache, Git merge driver)
- Tick vs Iteration separation
- Ralph Wiggum Loop threading across personas
- Learnings -> Policy promotion
- Operating principles (correctness first, agents fallible, fresh context, etc.)
- Strategy knobs (stale policy, conflict policy, tick cadence, bundle size, etc.)

### MVP Phasing

- **MVP1**: No LLM. Rust + ratatui TUI. Human-as-Coordinator. Proves the spine.
- **MVP2**: LLM as doc validator (read-only, safest entry point for intelligence)
- **MVP3+**: LLM implementers + reviewers

### MVP1 Scope

- TUI with Dashboard, WorkItems, Bundles, Ticks, Learnings views
- TaskStore records: Plan, Spec, Phase, WorkItem, Bundle, Tick, Learning, Lock
- Minimal worktree manager (create/cleanup worktrees, propose bundles)
- Manual/semi-automatic Integrator (configured shell command validation)
- No LLM. No parallelism. Serial but architecturally honest.

### FSM Architecture

- Hand-rolled enum + const transition table pattern (no crate)
- Three state machines: WorkItem, Bundle, Tick
- Role-based guards as data in transition table
- Per-state invariants via `validate_invariants_for()` match
- Serde-friendly: plain enum derives, JSONL round-trips cleanly
- Testable: exhaustive transition tests + Graphviz generation

### Open Questions for Design Doc

1. Crate layout (workspace structure?)
2. Exact TaskStore record schemas
3. TUI view specifications
4. Worktree lifecycle management details
5. Configuration format and defaults for strategy knobs
6. Observability/logging architecture
7. Error handling patterns
