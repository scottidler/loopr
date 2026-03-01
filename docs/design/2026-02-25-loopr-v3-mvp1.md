# Design Document: Loopr v3 — MVP1

**Author:** Scott Aidler
**Date:** 2026-02-25
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Loopr is a TUI-based "dev team in a box" that orchestrates software development work through a Plan → Spec → Phase → Code hierarchy using the Ralph Wiggum Loop pattern (fresh LLM context each iteration). MVP1 proves the orchestration spine — TaskStore persistence, three finite state machines (Work, Bundle, Tick), daemon-mediated correctness, IPC, and a ratatui TUI — with zero LLM involvement. The human acts as every persona via the TUI.

## Glossary

| Term | Definition |
|------|-----------|
| **Ralph Wiggum Loop** | An iteration pattern where each LLM invocation starts with a fresh context window. The agent has no memory of prior iterations — only the persistent data store (TaskStore) carries state forward. Named for the Simpsons character who famously forgets everything. |
| **Tick** | An immutable integration checkpoint identified by a Git SHA. Represents a verified, known-good state of the codebase. Monotonically increasing. |
| **Bundle** | A proposed change set produced in a Git worktree. Contains the diff, touched paths, claims about what changed, and verification results. |
| **Work** | A concrete, assignable unit of work within a Phase. The thing an Implementer actually does. |
| **Plan → Spec → Phase → Work** | The work decomposition hierarchy. Plan is the goal, Spec is the detailed design, Phase is a sequential implementation step, Work is an atomic task. |
| **Persona / Role** | A hat the human wears (MVP1) or an LLM agent plays (MVP3+). Coordinator, Integrator, Implementer each have different FSM transition permissions. |
| **TaskStore** | External Rust crate (`scottidler/taskstore`). Provides JSONL-as-truth, SQLite-as-cache persistence with Git merge drivers. Generic over any type implementing the `Record` trait. |

## Problem Statement

### Background

Coding agents today (Claude Code, Cursor, Copilot) operate in a single context window with no persistent memory across sessions and no coordination between parallel work streams. Steve Yegge's Gas Town/Beads project demonstrated that multi-agent coordination is possible but suffered from chaotic multi-writer file-based communication, race conditions, and nondeterministic behavior.

Two prior Loopr attempts exist:
- **v1** completed a 10-phase implementation but was a proof-of-concept with unsatisfactory architecture.
- **v2** built substantial infrastructure (daemon, IPC, TUI, LLM client, tool system — 65+ source files) but hit a wall due to a too-generic domain model (single `Loop` type) and premature complexity.

### Problem

There is no tool that provides **correct, observable, deterministic orchestration** of multi-stage development work with proper state machines, integration checkpoints, and worktree isolation. Gas Town proved the concept is valuable but did it with chaos. We need the same capability built on correctness guarantees.

### Goals

- Prove the Work → Bundle → Tick pipeline works end-to-end with human-driven state transitions
- Validate three FSMs (Work, Bundle, Tick) with role-based guards and per-state invariants
- Demonstrate worktree isolation for bundle production
- Provide a usable ratatui TUI for creating records, transitioning states, and observing the system
- Establish the daemon + IPC architecture as the single-authority correctness chokepoint
- Build on TaskStore (external crate) for persistence — no reinventing storage

### Non-Goals

- **No LLM integration** — MVP1 is explicitly LLM-free. The hardest problems are orchestration, not generation.
- **No automated tool execution** — humans run commands manually, bundle the results.
- **No parallelism** — serial execution, one actor at a time. The data model supports concurrency; MVP1 doesn't exercise it.
- **No spec/review/research swarms** — these plug into the backbone later (MVP3+).
- **No network features** — single-machine, single-user.
- **No fancy TUI** — functional over beautiful. Dashboard, lists, state transitions, keyboard navigation.

### Success Criteria

MVP1 is complete when a human can do the following through the TUI (or CLI), end-to-end, against a real Git repository:

1. Create a Plan → Spec → Phase → Work hierarchy
2. Create a Git worktree for a Work
3. (In a separate terminal) make changes in the worktree, commit them
4. Propose a Bundle from the worktree
5. Walk the Bundle through Proposed → Triaged → Reviewed → Accepted
6. Create a Tick, seal it, validate it (runs `cargo test` or equivalent), and publish it
7. See the published Tick's Git SHA
8. All invalid transitions are rejected with clear error messages
9. Role switching changes available actions
10. Daemon survives TUI disconnect and reconnects cleanly
11. Bundle proposal is rejected when `base_tick_id` is behind the latest Published Tick (staleness guard), forcing a worktree refresh before resubmission

## Proposed Solution

### Overview

A single Rust binary (`loopr`) that operates in two modes:

1. **Daemon mode** — long-running Tokio process that owns all mutable state (TaskStore), validates FSM transitions, manages worktrees, and broadcasts events.
2. **TUI/CLI mode** — thin client that connects to the daemon over a Unix socket, sends commands, receives events, and renders state via ratatui.

The daemon is the **single authority**. Every state mutation goes through it. The TUI never touches TaskStore directly. This is the fundamental architectural difference from Gas Town's multi-writer chaos.

### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                      loopr binary                        │
│                                                          │
│  ┌──────────────┐     Unix Socket      ┌──────────────┐ │
│  │   TUI/CLI    │◄──── NDJSON ────────►│    Daemon     │ │
│  │  (ratatui)   │     IPC Protocol     │   (Tokio)     │ │
│  │              │                      │              │ │
│  │ • Renders    │   DaemonRequest ──►  │ • Validates   │ │
│  │ • Input      │                      │   FSM trans.  │ │
│  │ • No state   │  ◄── DaemonResponse  │ • Role guards │ │
│  │              │  ◄── DaemonEvent     │ • Worktrees   │ │
│  └──────────────┘                      │ • TaskStore   │ │
│                                        └──────┬───────┘ │
└───────────────────────────────────────────────┼─────────┘
                                                │
                              ┌─────────────────┼──────────┐
                              │           TaskStore         │
                              │                             │
                              │  JSONL (source of truth)    │
                              │  SQLite (query cache)       │
                              │  Git merge driver           │
                              └─────────────────────────────┘
```

#### Component Responsibilities

| Component | Responsibility |
|-----------|---------------|
| **Daemon** | Single process owning all mutable state. Validates every FSM transition. Manages worktrees. Writes to TaskStore. Broadcasts events to connected clients. |
| **TUI** | Pure display + input. Subscribes to daemon events. Sends user commands as IPC requests. Never touches storage. |
| **CLI** | Headless alternative to TUI. Same IPC protocol. Enables scripting. |
| **TaskStore** | External crate. JSONL as source of truth, SQLite as read cache. Git merge driver for collaboration. Generic `Record` trait. |
| **FSM Engine** | Hand-rolled enum + const transition table. Validates (from, to, role) tuples. Enforces per-state invariants. |
| **WorktreeManager** | Creates/cleans Git worktrees for bundle production. Maps work items to isolated branches. |

### Crate Layout

Single crate for MVP1 (no workspace). Internal module organization:

```
loopr/
├── Cargo.toml
├── build.rs
├── src/
│   ├── main.rs              # Entry point: fork-to-daemon or connect-as-client
│   ├── lib.rs               # Module declarations
│   │
│   ├── domain/              # Core domain types and FSMs
│   │   ├── mod.rs
│   │   ├── plan.rs          # Plan record
│   │   ├── spec.rs          # Spec record
│   │   ├── phase.rs         # Phase record
│   │   ├── work.rs     # Work record + FSM
│   │   ├── bundle.rs        # Bundle record + FSM
│   │   ├── tick.rs          # Tick record + FSM
│   │   ├── learning.rs      # Learning record
│   │   ├── lock.rs          # Lock record (advisory)
│   │   ├── role.rs          # Role enum
│   │   └── transition.rs    # Shared transition validation logic
│   │
│   ├── daemon/              # Daemon process
│   │   ├── mod.rs           # Daemon startup, main select! loop
│   │   ├── context.rs       # DaemonContext (shared state hub)
│   │   └── handlers.rs      # IPC request handlers
│   │
│   ├── ipc/                 # Inter-process communication
│   │   ├── mod.rs
│   │   ├── protocol.rs      # Request/Response/Event types
│   │   ├── server.rs        # Unix socket server (daemon side)
│   │   ├── client.rs        # Unix socket client (TUI/CLI side)
│   │   └── codec.rs         # NDJSON codec (tokio_util)
│   │
│   ├── tui/                 # Terminal UI
│   │   ├── mod.rs
│   │   ├── app.rs           # App state, event loop
│   │   ├── views/           # One module per view
│   │   │   ├── mod.rs
│   │   │   ├── dashboard.rs
│   │   │   ├── works.rs
│   │   │   ├── bundles.rs
│   │   │   ├── ticks.rs
│   │   │   └── learnings.rs
│   │   └── input.rs         # Keyboard handling
│   │
│   ├── worktree/            # Git worktree management
│   │   ├── mod.rs
│   │   └── manager.rs
│   │
│   ├── cli/                 # CLI commands (thin IPC clients)
│   │   ├── mod.rs           # Clap derive structs (Cli enum)
│   │   └── dispatch.rs      # Subcommand → IPC request mapping
│   │
│   ├── config.rs            # Configuration
│   ├── error.rs             # Error types
│   └── id.rs                # ID generation
│
├── docs/
│   ├── design/
│   └── yegge/
│
└── tests/
    └── integration/
```

### Data Model

All records implement TaskStore's `Record` trait:

```rust
pub trait Record: Serialize + for<'de> Deserialize<'de> + Clone + Send + Sync + 'static {
    fn id(&self) -> &str;
    fn updated_at(&self) -> i64;
    fn collection_name() -> &'static str;
    fn indexed_fields(&self) -> HashMap<String, IndexValue>;
}
```

#### Records

**Plan** — Top-level objective. Contains markdown description and acceptance criteria.

```rust
pub struct Plan {
    pub id: String,
    pub title: String,
    pub description: String,        // Markdown
    pub acceptance_criteria: String, // Markdown
    pub status: PlanStatus,         // Draft | Active | Complete | Abandoned
    pub created_at: i64,
    pub updated_at: i64,
}
```

**Spec** — Detailed specification derived from a Plan.

```rust
pub struct Spec {
    pub id: String,
    pub plan_id: String,            // Parent reference
    pub title: String,
    pub description: String,        // Markdown
    pub status: SpecStatus,         // Draft | Active | Complete | Abandoned
    pub created_at: i64,
    pub updated_at: i64,
}
```

**Phase** — Implementation phase within a Spec. Ordered.

```rust
pub struct Phase {
    pub id: String,
    pub spec_id: String,            // Parent reference
    pub title: String,
    pub description: String,        // Markdown
    pub order: u32,                 // Execution order within spec
    pub status: PhaseStatus,        // Draft | Active | Complete | Abandoned
    pub created_at: i64,
    pub updated_at: i64,
}
```

**Work** — Concrete unit of work within a Phase. Has a full FSM.

```rust
pub struct Work {
    pub id: String,
    pub phase_id: String,           // Parent reference
    pub title: String,
    pub description: String,        // Markdown
    pub assignee: Option<String>,   // Who's working on it
    pub status: WorkStatus,
    pub resource_tags: Vec<String>, // Files/modules this touches
    pub dependencies: Vec<String>,  // IDs of prerequisite Works
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum WorkStatus {
    Draft,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Abandoned,
}
```

**Bundle** — A proposed change set produced from a worktree. Has a full FSM.

```rust
pub struct Bundle {
    pub id: String,
    pub work_id: String,       // Which Work this fulfills
    pub base_tick_id: Option<String>,// Which Tick this was based on (None for first bundle before any tick)
    pub branch_name: String,        // Git branch in worktree
    pub touched_paths: Vec<String>, // Files modified
    pub claims: String,             // What this bundle asserts it does
    pub verification: String,       // What validation was run
    pub status: BundleStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum BundleStatus {
    Proposed,
    Triaged,
    Reviewed,
    Accepted,
    Integrating,
    Merged,
    Rejected,
    Superseded,
}
```

**Tick** — Immutable integration checkpoint. A published Tick has a Git SHA.

```rust
pub struct Tick {
    pub id: String,
    pub number: u32,                // Monotonically increasing
    pub integration_sha: Option<String>, // Git SHA when published
    pub bundle_ids: Vec<String>,    // Bundles included in this tick
    pub validation_log: String,     // CI output
    pub status: TickStatus,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum TickStatus {
    Open,
    Sealing,
    Validating,
    Published,
    Failed,
}
```

**Learning** — Insight captured during work. Can be promoted to Policy.

```rust
pub struct Learning {
    pub id: String,
    pub source_id: String,          // What produced this learning
    pub scope: LearningScope,       // Work | Phase | Spec | Plan | Global
    pub content: String,            // The insight
    pub reinforcements: u32,        // Times independently confirmed
    pub contradictions: u32,        // Times contradicted
    pub promoted: bool,             // Is this a Policy?
    pub created_at: i64,
    pub updated_at: i64,
}
```

**Lock** — Advisory lock on a resource. MVP1 uses soft locks.

```rust
pub struct Lock {
    pub id: String,
    pub resource: String,           // What's locked (file path, module name)
    pub holder_id: String,          // Who holds it (work item ID)
    pub granted_by: String,         // Who granted it (coordinator)
    pub status: LockStatus,         // Active | Released | Expired
    pub created_at: i64,
    pub updated_at: i64,
}
```

#### Collection Names and Indexed Fields

| Record | Collection | Indexed Fields |
|--------|-----------|----------------|
| Plan | `plans` | `status` |
| Spec | `specs` | `plan_id`, `status` |
| Phase | `phases` | `spec_id`, `status`, `order` (as `IndexValue::Int(i64)`) |
| Work | `works` | `phase_id`, `status`, `assignee` |
| Bundle | `bundles` | `work_id`, `status`, `base_tick_id` |
| Tick | `ticks` | `status`, `number` |
| Learning | `learnings` | `source_id`, `scope`, `promoted` |
| Lock | `locks` | `resource`, `holder_id`, `status` |

### FSM Design

Hand-rolled enum + const transition table. No external crate.

#### Pattern

```rust
pub struct TransitionRule<S> {
    pub from: S,            // Source state
    pub to: S,              // Target state
    pub role: Option<Role>, // Required role (None = any)
}

pub fn validate_transition<S: PartialEq + Copy + Debug>(
    current: S,
    target: S,
    role: Role,
    rules: &[TransitionRule<S>],
) -> Result<()> {
    let allowed = rules.iter().any(|r| {
        r.from == current && r.to == target
            && r.role.map_or(true, |required| required == role)
    });
    if !allowed {
        return Err(LooprError::InvalidTransition {
            from: format!("{:?}", current),
            to: format!("{:?}", target),
            role: format!("{:?}", role),
        });
    }
    Ok(())
}
```

#### Work FSM

```
                    ┌──────────┐
                    │  Draft   │
                    └────┬─────┘
                         │ Coordinator
                    ┌────▼─────┐
              ┌─────│  Ready   │◄────────────┐
              │     └────┬─────┘             │
              │          │ Coordinator        │ Coordinator
              │     ┌────▼──────┐       ┌────┴─────┐
              │     │InProgress │──────►│ Blocked  │
              │     └────┬──────┘       └──────────┘
              │          │ Implementer
              │     ┌────▼─────┐
              │     │ InReview │──┐
              │     └────┬─────┘  │ Coordinator (rejection)
              │          │        │ → back to InProgress
              │          │ Integrator
              │     ┌────▼──────┐
              │     │Integrated │
              │     └────┬──────┘
              │          │ Coordinator
              │     ┌────▼─────┐
              │     │   Done   │
              │     └──────────┘
              │
              │  (Any → Abandoned, Coordinator only)
              └────►┌───────────┐
                    │ Abandoned │
                    └───────────┘
```

**Transition Table:**

```rust
const WORK_ITEM_TRANSITIONS: &[TransitionRule<WorkStatus>] = &[
    TransitionRule { from: Draft,      to: Ready,      role: Some(Coordinator) },
    TransitionRule { from: Ready,      to: InProgress, role: Some(Coordinator) },
    TransitionRule { from: InProgress, to: Blocked,    role: None },
    TransitionRule { from: Blocked,    to: Ready,      role: Some(Coordinator) },
    TransitionRule { from: InProgress, to: InReview,   role: Some(Implementer) },
    TransitionRule { from: InReview,   to: InProgress, role: Some(Coordinator) },
    TransitionRule { from: InReview,   to: Integrated, role: Some(Integrator) },
    TransitionRule { from: Integrated, to: Done,       role: Some(Coordinator) },
    // Abandoned from any non-terminal state
    TransitionRule { from: Draft,      to: Abandoned,  role: Some(Coordinator) },
    TransitionRule { from: Ready,      to: Abandoned,  role: Some(Coordinator) },
    TransitionRule { from: InProgress, to: Abandoned,  role: Some(Coordinator) },
    TransitionRule { from: Blocked,    to: Abandoned,  role: Some(Coordinator) },
    TransitionRule { from: InReview,   to: Abandoned,  role: Some(Coordinator) },
    TransitionRule { from: Integrated, to: Abandoned,  role: Some(Coordinator) },
];
```

**Invariants:**
1. Single assignee when InProgress or InReview
2. Ready implies description is non-empty (scope is bounded)
3. InReview implies exactly one active Bundle for this Work
4. Dependencies must be acyclic (validated on create/update, not on transition)
5. Work must have non-empty resource_tags before transitioning to Ready+

#### Bundle FSM

```rust
const BUNDLE_TRANSITIONS: &[TransitionRule<BundleStatus>] = &[
    TransitionRule { from: Proposed,    to: Triaged,     role: Some(Coordinator) },
    TransitionRule { from: Triaged,     to: Reviewed,    role: Some(Coordinator) },
    TransitionRule { from: Reviewed,    to: Accepted,    role: Some(Coordinator) },
    TransitionRule { from: Accepted,    to: Integrating, role: Some(Integrator) },
    TransitionRule { from: Integrating, to: Merged,      role: Some(Integrator) },
    TransitionRule { from: Integrating, to: Rejected,    role: Some(Integrator) },
    // Early rejection
    TransitionRule { from: Proposed,    to: Rejected,    role: Some(Coordinator) },
    TransitionRule { from: Triaged,     to: Rejected,    role: Some(Coordinator) },
    TransitionRule { from: Reviewed,    to: Rejected,    role: Some(Coordinator) },
    // Superseded (from any non-final state)
    TransitionRule { from: Proposed,    to: Superseded,  role: Some(Coordinator) },
    TransitionRule { from: Triaged,     to: Superseded,  role: Some(Coordinator) },
    TransitionRule { from: Reviewed,    to: Superseded,  role: Some(Coordinator) },
    TransitionRule { from: Accepted,    to: Superseded,  role: Some(Coordinator) },
    TransitionRule { from: Integrating, to: Superseded,  role: Some(Coordinator) },
];
```

**Invariants:**
1. Bundle must declare its base (`base_tick_id`) unless no Tick has been published yet (bootstrap case)
2. **Staleness guard:** Bundle proposal is rejected if `base_tick_id` does not match the latest Published Tick's ID. The Implementer must refresh their worktree (rebase onto the new baseline) and re-propose. This is a hard guard, not a warning — stale bundles cannot enter the pipeline.
3. At most one Accepted bundle per Work at a time
4. Bundle cannot touch locked resources it doesn't own
5. Verification metadata is required for Reviewed+

#### Tick FSM

```rust
const TICK_TRANSITIONS: &[TransitionRule<TickStatus>] = &[
    TransitionRule { from: Open,       to: Sealing,    role: Some(Integrator) },
    TransitionRule { from: Sealing,    to: Validating, role: Some(Integrator) },
    TransitionRule { from: Validating, to: Published,  role: Some(Integrator) },
    TransitionRule { from: Validating, to: Failed,     role: Some(Integrator) },
];
```

**Invariants:**
1. Only one Tick can be in Sealing or Validating at a time
2. Published Tick has exactly one integration_sha
3. Tick records exactly which bundle_ids it attempted
4. A Failed Tick must have a non-empty validation_log
5. Publishing a Tick advances the baseline. Any subsequent Bundle proposal with a `base_tick_id` older than this Tick is rejected by the staleness guard (see Bundle invariant #2). Automated cascade notification to in-progress Works is MVP3+ — in MVP1 the human discovers staleness when their next bundle proposal is rejected, then refreshes their worktree.

#### Roles

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Role {
    Coordinator,  // PM/EM: assigns work, grants locks, makes decisions
    Integrator,   // Release engineer: merges bundles, publishes ticks
    Implementer,  // Engineer: owns worktrees, produces bundles
    Reviewer,     // Critic: reviews bundles (MVP3+, not used in MVP1)
    Researcher,   // Librarian: search, summarize (MVP3+, not used in MVP1)
}
```

In MVP1, the human switches between Coordinator, Integrator, and Implementer via the TUI (press `r` to cycle). The current role determines which FSM transitions are available in the action bar. Reviewer and Researcher are defined for forward compatibility but have no transitions in MVP1.

#### Plan / Spec / Phase Status

Plan, Spec, and Phase use a simple four-state status with lightweight transition enforcement (Coordinator-only for all transitions):

```rust
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum HierarchyStatus {
    Draft,
    Active,
    Complete,
    Abandoned,
}

const HIERARCHY_TRANSITIONS: &[TransitionRule<HierarchyStatus>] = &[
    TransitionRule { from: Draft,    to: Active,    role: Some(Coordinator) },
    TransitionRule { from: Active,   to: Complete,  role: Some(Coordinator) },
    TransitionRule { from: Draft,    to: Abandoned, role: Some(Coordinator) },
    TransitionRule { from: Active,   to: Abandoned, role: Some(Coordinator) },
];
```

This shares the same `TransitionRule` / `validate_transition` machinery as the three main FSMs. Plan, Spec, and Phase all use `HierarchyStatus` and `HIERARCHY_TRANSITIONS`. `PlanStatus`, `SpecStatus`, `PhaseStatus` are type aliases for `HierarchyStatus`.

### IPC Protocol

NDJSON over Unix socket at `~/.loopr/daemon.sock`.

#### Message Types

```rust
// Client → Daemon
pub struct DaemonRequest {
    pub id: u64,
    pub method: String,
    pub params: serde_json::Value,
}

// Daemon → Client (response to a request)
pub struct DaemonResponse {
    pub id: u64,
    pub result: Option<serde_json::Value>,
    pub error: Option<RpcError>,
}

// Daemon → Client (unsolicited push)
pub struct DaemonEvent {
    pub event: String,
    pub data: serde_json::Value,
}
```

#### Method Catalog (MVP1)

**Record CRUD:**
- `plan.create`, `plan.get`, `plan.list`, `plan.update`
- `spec.create`, `spec.get`, `spec.list`, `spec.update`
- `phase.create`, `phase.get`, `phase.list`, `phase.update`
- `work.create`, `work.get`, `work.list`, `work.update`
- `bundle.create`, `bundle.get`, `bundle.list`, `bundle.update`
- `tick.create`, `tick.get`, `tick.list`, `tick.update`
- `learning.create`, `learning.get`, `learning.list`, `learning.update`
- `lock.create`, `lock.get`, `lock.list`, `lock.release`

**FSM Transitions:**
- `plan.transition { id, target_status }` — HierarchyStatus transitions (Draft → Active → Complete/Abandoned)
- `spec.transition { id, target_status }` — HierarchyStatus transitions
- `phase.transition { id, target_status }` — HierarchyStatus transitions
- `work.transition { id, target_status, role }`
- `bundle.transition { id, target_status, role }`
- `tick.transition { id, target_status, role }`

**Worktree Operations:**
- `worktree.create { work_id, base_ref }`
- `worktree.refresh { work_id }` — rebase worktree onto latest Published Tick's SHA (required to clear staleness)
- `worktree.list`
- `worktree.cleanup { work_id }`

**Integrator Operations:**
- `integrator.validate { tick_id }` — runs configured validation commands
- `integrator.publish { tick_id }` — publishes tick if validation passed

**System:**
- `system.handshake { client_version }` — version check
- `system.init` — create TaskStore collections and default config (idempotent)
- `system.status` — daemon health + current Tick SHA, record counts by status, stale work items
- `system.shutdown` — graceful shutdown

#### Event Catalog (MVP1)

- `record.created { collection, id }`
- `record.updated { collection, id }`
- `record.deleted { collection, id }`
- `transition.completed { collection, id, from, to, role }`
- `transition.rejected { collection, id, from, to, role, reason }`
- `tick.published { tick_id, sha }`
- `bundle.rejected_stale { bundle_work_id, base_tick_id, latest_tick_id }` — bundle proposal rejected due to staleness guard
- `worktree.created { work_id, path }`
- `worktree.cleaned { work_id }`
- `validation.started { tick_id }`
- `validation.completed { tick_id, success, log }`

### Daemon Architecture

#### DaemonContext

```rust
pub struct DaemonContext {
    pub store: Store,                           // TaskStore instance
    pub worktree_manager: WorktreeManager,
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub config: Config,
}
```

**TaskStore location:** `Store::open()` is called with the target repo's root path. TaskStore creates `.taskstore/` there, containing JSONL files (committed to Git) and `taskstore.db` (gitignored). This means Loopr's records live alongside the target repo's code and are version-controlled.

No LLM client, no tool router, no prompt engine. MVP1 is clean.

#### Main Loop

```rust
async fn daemon_main(ctx: Arc<RwLock<DaemonContext>>) -> Result<()> {
    let socket_path = {
        let c = ctx.read().await;
        c.config.daemon.socket_path.clone()
    };
    let listener = UnixListener::bind(&socket_path)?;

    loop {
        tokio::select! {
            Ok((stream, _)) = listener.accept() => {
                let ctx = ctx.clone();
                tokio::spawn(handle_client(ctx, stream));
            }
            _ = signal::ctrl_c() => {
                // Graceful shutdown: clean up socket, PID file
                let _ = std::fs::remove_file(&socket_path);
                break;
            }
        }
    }
    Ok(())
}
```

#### Handler Pattern

Each IPC method maps to a handler function that:
1. Deserializes params
2. Acquires write lock on DaemonContext (if mutating)
3. Validates the operation (FSM transition, invariants)
4. Writes to TaskStore
5. Broadcasts event
6. Returns result

```rust
async fn handle_work_transition(
    ctx: &Arc<RwLock<DaemonContext>>,
    id: &str,
    target: WorkStatus,
    role: Role,
) -> Result<DaemonResponse> {
    let mut ctx = ctx.write().await;

    let mut item: Work = ctx.store.get(id)?
        .ok_or(LooprError::NotFound)?;

    let from = item.status;
    item.transition(target, role)?;  // FSM validation + invariant checks
    ctx.store.update(item.clone())?;

    // Broadcast is non-blocking (receivers may lag)
    let _ = ctx.event_tx.send(DaemonEvent::transition_completed(
        "works", id, from, target, role,
    ));

    Ok(DaemonResponse::ok(serde_json::to_value(&item)?))
}
```

### CLI Design

The `loopr` binary operates in three modes depending on how it's invoked:

1. **`loopr`** (no args) — start/connect TUI
2. **`loopr <subcommand>`** — send one IPC request to the daemon, print result, exit
3. **`loopr daemon`** — run as daemon (normally fork-started, not user-invoked)

All CLI subcommands are thin IPC clients. They connect to the daemon's Unix socket, send a single request, print the response, and exit. If the daemon isn't running, the CLI forks it first (same as the TUI). This means `loopr plan create ...` and pressing `n` in the TUI's Dashboard do the exact same thing — both send `plan.create` to the daemon.

#### Subcommand → IPC Mapping

**Setup & Status:**
```
loopr init                                    # creates .taskstore/ collections + default config
loopr status                                  # → system.status (tick SHA, record counts, stale items)
```

**Hierarchy CRUD (as Coordinator):**
```
loopr plan create --title "..."               # → plan.create
loopr plan list                               # → plan.list
loopr spec create --plan <id> --title "..."   # → spec.create
loopr phase create --spec <id> --title "..."  # → phase.create
loopr work create --phase <id> --title "..." --tags src/foo.rs,Cargo.toml
                                              # → work.create
```

**FSM Transitions:**
```
loopr plan transition <id> <status>           # → plan.transition { id, target_status, role }
loopr work transition <id> <status>      # → work.transition { id, target_status, role }
loopr bundle transition <id> <status>         # → bundle.transition { id, target_status, role }
loopr tick transition <id> <status>           # → tick.transition { id, target_status, role }
```

The `role` is read from the CLI's current role setting (flag `--as <role>` or persisted in config). Invalid role for the requested transition → daemon rejects it.

**Worktree Operations (as Implementer):**
```
loopr worktree create <work-id>          # → worktree.create { work_id, base_ref: latest tick SHA }
loopr worktree refresh <work-id>         # → worktree.refresh { work_id } (rebase onto latest tick)
loopr worktree list                           # → worktree.list
loopr worktree cleanup <work-id>         # → worktree.cleanup { work_id }
```

**Bundle Operations (as Implementer):**
```
loopr bundle propose <work-id> --notes "..."
                                              # → bundle.create (auto-collects git diff, touched paths, base_tick_id)
                                              # staleness guard: rejected if base_tick_id is behind latest Published Tick
```

**Tick Operations (as Integrator):**
```
loopr tick create                             # → tick.create
loopr tick add-bundle <tick-id> <bundle-id>   # → tick.update (add bundle to tick's bundle_ids)
loopr tick publish <tick-id>                  # → tick.transition seal → tick.transition validating
                                              #   daemon runs validation commands
                                              #   pass: Published (SHA recorded) / fail: Failed (log shown)
```

**System:**
```
loopr role [coordinator|integrator|implementer]  # set current role (persisted in config)
loopr shutdown                                   # → system.shutdown
```

`loopr tick publish` is a convenience that chains seal → validating transitions. The individual transitions are also available via `loopr tick transition` for fine-grained control.

#### CLI Module

Clap derive structs in `src/cli/mod.rs`:

```rust
#[derive(Parser)]
#[command(name = "loopr")]
pub enum Cli {
    /// Start the TUI (default when no subcommand)
    Tui,
    /// Run as daemon (normally fork-started)
    Daemon,
    /// Initialize TaskStore collections and config
    Init,
    /// Show current status (tick SHA, record counts, stale items)
    Status,
    /// Set or show current role
    Role { role: Option<Role> },
    /// Plan operations
    Plan {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Spec operations
    Spec {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Phase operations
    Phase {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Work item operations
    Work {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Bundle operations
    Bundle {
        #[command(subcommand)]
        cmd: BundleCmd,
    },
    /// Tick operations
    Tick {
        #[command(subcommand)]
        cmd: TickCmd,
    },
    /// Worktree operations
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Graceful shutdown
    Shutdown,
}
```

Each subcommand handler: connect to daemon socket → serialize IPC request → send → read response → print result or error → exit. No domain logic in the CLI module.

### TUI Design

#### Views

Five views, cycled with Tab:

1. **Dashboard** — Current tick number, active plan/spec/phase, queue counts (work items by status, bundles by status), role selector.

2. **Work Items** — List view with status badges. Detail panel showing description, assignee, dependencies, resource tags. Action bar for transitions (filtered by current role).

3. **Bundles** — List grouped by work item. Shows base tick, touched paths, verification status. Action bar for transitions.

4. **Ticks** — History list with tick number, SHA (if published), status, included bundles. Action to create new tick, seal, validate, publish.

5. **Learnings** — List with scope, reinforcement count. Promote/demote actions.

#### TUI Architecture

```rust
pub struct App {
    pub current_view: View,
    pub current_role: Role,
    pub ipc_client: IpcClient,
    pub state: AppState,  // Cached records from daemon events
}

pub struct AppState {
    pub plans: Vec<Plan>,
    pub specs: Vec<Spec>,
    pub phases: Vec<Phase>,
    pub works: Vec<Work>,
    pub bundles: Vec<Bundle>,
    pub ticks: Vec<Tick>,
    pub learnings: Vec<Learning>,
    pub locks: Vec<Lock>,
}
```

The TUI subscribes to daemon events and keeps `AppState` in sync. On startup, it fetches all records via `*.list` calls.

#### Keyboard Bindings

| Key | Action |
|-----|--------|
| Tab | Next view |
| Shift+Tab | Previous view |
| j/k or ↑/↓ | Navigate list |
| Enter | Open detail / confirm action |
| Esc | Close detail / cancel |
| n | New record (context-dependent) |
| t | Transition state (opens picker) |
| r | Switch role |
| q | Quit |
| ? | Help |

### Worktree Management

Minimal for MVP1. The daemon manages worktrees:

```rust
pub struct WorktreeManager {
    pub repo_path: PathBuf,
    pub worktree_dir: PathBuf,  // .worktrees/
}

impl WorktreeManager {
    /// Create a worktree for a work item
    pub fn create(&self, work_id: &str, base_ref: &str) -> Result<PathBuf> {
        let path = self.worktree_dir.join(work_id);
        let branch = format!("agent/{}", work_id);
        // git worktree add <path> -b <branch> <base_ref>
        Command::new("git")
            .args(["worktree", "add", &path.to_string_lossy(), "-b", &branch, base_ref])
            .current_dir(&self.repo_path)
            .status()?;
        Ok(path)
    }

    /// Refresh a worktree to the latest Published Tick's SHA (clears staleness)
    pub fn refresh(&self, work_id: &str, new_base_ref: &str) -> Result<()> {
        let path = self.worktree_dir.join(work_id);
        // git -C <worktree> rebase <new_base_ref>
        Command::new("git")
            .args(["-C", &path.to_string_lossy(), "rebase", new_base_ref])
            .status()?;
        Ok(())
    }

    /// Clean up a worktree after bundle is merged or abandoned
    pub fn cleanup(&self, work_id: &str) -> Result<()> {
        let path = self.worktree_dir.join(work_id);
        Command::new("git")
            .args(["worktree", "remove", &path.to_string_lossy()])
            .current_dir(&self.repo_path)
            .status()?;
        Ok(())
    }

    /// List active worktrees
    pub fn list(&self) -> Result<Vec<WorktreeInfo>> { /* ... */ }
}
```

### Integrator (Semi-Automatic)

For MVP1, the Integrator is human-triggered but validation is automated:

```rust
pub struct IntegratorConfig {
    pub validation_commands: Vec<String>,
    // e.g., ["cargo fmt --check", "cargo clippy -- -D warnings", "cargo test"]
}
```

When the human triggers `integrator.validate`:
1. Tick must be in Sealing state (Integrator transitions Open → Sealing first)
2. Daemon transitions Sealing → Validating, then runs each command in sequence
3. Captures stdout/stderr into `Tick.validation_log`
4. If all pass: Tick transitions Validating → Published, integration_sha is recorded
5. If any fail: Tick transitions Validating → Failed, log explains why

The human drives each step: create tick (Open), seal it (Sealing), validate (Validating → Published/Failed). This maps directly to the Tick FSM.

### Client Fork-to-Daemon

Single binary, auto-start:

```rust
fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Daemon { foreground }) => {
            run_daemon(foreground)
        }
        _ => {
            // Try to connect to existing daemon
            if let Ok(client) = connect_to_daemon() {
                run_client(client, cli)
            } else {
                // Auto-start daemon in background
                fork_daemon()?;
                // Wait briefly, then connect
                let client = connect_to_daemon()?;
                run_client(client, cli)
            }
        }
    }
}
```

PID file at `~/.loopr/daemon.pid`. Liveness check via `kill(pid, 0)`. Version file at `~/.loopr/daemon.version` for mismatch detection.

**Stale socket cleanup:** On daemon startup, if `daemon.sock` exists, check `daemon.pid`. If the PID is dead (`kill(pid, 0)` fails), remove the stale socket and PID file before binding.

### Configuration

```toml
# ~/.config/loopr/config.toml (or .loopr/config.toml in project root)

[daemon]
socket_path = "~/.loopr/daemon.sock"
pid_path = "~/.loopr/daemon.pid"

[project]
repo_path = "/path/to/target/repo"
worktree_dir = ".worktrees"

[integrator]
validation_commands = [
    "cargo fmt --check",
    "cargo clippy -- -D warnings",
    "cargo test",
]
```

### Implementation Plan

#### Phase 1: Foundation

- Domain types: all 8 record structs with serde derives and Record trait implementations
- FSM engine: transition tables, validation function, role enum
- Error types
- ID generation
- Config parsing

**Validation:** `cargo test` — unit tests for every FSM transition (valid and invalid), record serialization round-trips, invariant checks.

#### Phase 2: Daemon Core

- DaemonContext
- Unix socket server with NDJSON codec
- Client connection handling (spawn per client)
- Version handshake
- PID file lifecycle
- Broadcast channel for events

**Validation:** Integration test — start daemon, connect client, send handshake, receive response.

#### Phase 3: Handlers + Worktree Manager

- CRUD handlers for all 8 record types
- FSM transition handlers with role checking
- Event broadcasting on state changes
- WorktreeManager: create/list/cleanup worktrees
- Worktree-related handlers: `worktree.create`, `worktree.list`, `worktree.cleanup`

**Validation:** Integration tests — create records, transition states, verify events broadcast, create and cleanup a worktree.

#### Phase 4: TUI

- App struct and event loop
- IPC client connection
- Dashboard view
- Work Items list + detail view
- Bundles view
- Ticks view
- Learnings view
- Keyboard navigation
- Role switching

**Validation:** Manual testing — full end-to-end workflow through TUI.

#### Phase 5: Integration Pipeline

- Bundle proposal workflow: collect touched paths and commit range from worktree, create Bundle record
- Integrator validation command runner (sequential shell commands, capture output)
- Tick sealing, validation, and publishing (record integration SHA on success)
- End-to-end: Plan → Spec → Phase → Work → worktree → Bundle → Tick

**Validation:** Integration test — full pipeline from plan creation to tick publication.

#### Phase 6: CLI + Polish

- CLI commands via IPC (headless operation)
- Crash recovery (detect orphaned InProgress Works and Integrating Bundles on daemon startup)
- Graceful shutdown (SIGTERM → wait → SIGKILL)
- Logging (env_logger)
- Error messages and edge case handling

**Validation:** CLI script that exercises the full pipeline without TUI.

### End-to-End Workflow (Concrete Example)

Here's what using MVP1 looks like for a human completing one full cycle:

```
1. PLAN (as Coordinator)
   └─ Press 'n' on Dashboard → create Plan "Add user authentication"
   └─ Plan starts as Draft → transition to Active

2. SPEC (as Coordinator)
   └─ Navigate to Plan detail → press 'n' → create Spec "JWT-based auth with refresh tokens"
   └─ Spec starts as Draft → transition to Active

3. PHASE (as Coordinator)
   └─ Create Phase 1 "Token generation and validation" under the Spec
   └─ Create Phase 2 "Login/logout endpoints" under the Spec
   └─ Transition Phase 1 to Active

4. WORK ITEM (as Coordinator)
   └─ Create Work "Implement JWT signing" under Phase 1
   └─ Set resource_tags: ["src/auth/jwt.rs", "Cargo.toml"]
   └─ Transition Draft → Ready
   └─ Transition Ready → InProgress (assigns to self)

5. WORKTREE (as Implementer)
   └─ Press 'w' to create worktree → daemon runs:
      git worktree add .worktrees/wi-20260225-a1b2 -b agent/wi-20260225-a1b2 HEAD
   └─ Human opens a terminal, cd's into .worktrees/wi-20260225-a1b2
   └─ Human writes code, runs tests manually, commits

6. BUNDLE (as Implementer)
   └─ Press 'b' to propose bundle → daemon collects:
      - touched_paths from git diff
      - branch_name from worktree
      - base_tick_id (None if first bundle, or latest Published tick)
   └─ Human fills in claims ("Added JWT sign/verify") and verification ("cargo test passes")
   └─ Bundle created as Proposed
   └─ Transition Work InProgress → InReview

7. REVIEW (as Coordinator)
   └─ Navigate to Bundles view → see Proposed bundle
   └─ Transition Proposed → Triaged → Reviewed → Accepted

8. INTEGRATION (as Integrator)
   └─ Create new Tick (Open)
   └─ Add the Accepted bundle to the Tick
   └─ Transition Open → Sealing
   └─ Transition Sealing → Validating (triggers validation commands)
      Daemon runs: cargo fmt --check && cargo clippy -- -D warnings && cargo test
   └─ If pass: Validating → Published (SHA recorded)
   └─ If fail: Validating → Failed (log shows why)

9. COMPLETION (as Coordinator)
   └─ Transition Work Integrated → Done
   └─ Clean up worktree
   └─ Move to next Work

10. STALENESS GUARD (proof point — do this on a second Work)
   └─ Start a second Work, create its worktree, make changes, commit
   └─ Meanwhile, Tick from step 8 has been Published (baseline advanced)
   └─ Press 'b' to propose bundle → REJECTED: base_tick_id is behind latest Published Tick
   └─ Press 'f' to refresh worktree (rebases onto new baseline)
   └─ Press 'b' again → bundle accepted with updated base_tick_id
```

### IPC Message Discrimination

On the Unix socket, three message types are interleaved. Each is a single JSON line. The codec distinguishes them by field presence:

- **Request** (client → daemon): has `"method"` field → `{ "id": 1, "method": "work.get", "params": {...} }`
- **Response** (daemon → client): has `"id"` but no `"method"` → `{ "id": 1, "result": {...} }` or `{ "id": 1, "error": {...} }`
- **Event** (daemon → client): has `"event"` field → `{ "event": "transition.completed", "data": {...} }`

This is the same discrimination pattern as JSON-RPC 2.0 (which uses `"method"` for requests, `"result"`/`"error"` for responses, and notifications have `"method"` but no `"id"`). We simplify by using a distinct `"event"` field for push notifications.

### TUI Event Loop

The TUI must poll two sources concurrently: terminal input (keyboard/mouse via crossterm) and IPC events (from the daemon). This is achieved with a Tokio `select!` in the TUI's main loop:

```rust
loop {
    tokio::select! {
        // Terminal event (keyboard, mouse, resize)
        Some(event) = terminal_events.next() => {
            match event {
                TerminalEvent::Key(key) => handle_key(&mut app, key).await?,
                TerminalEvent::Resize(w, h) => app.resize(w, h),
                _ => {}
            }
        }
        // IPC event from daemon
        Some(msg) = ipc_rx.next() => {
            match msg {
                IpcMessage::Response(resp) => app.handle_response(resp),
                IpcMessage::Event(evt) => app.handle_event(evt),
            }
        }
    }
    app.render(&mut terminal)?;
}
```

The `terminal_events` stream wraps crossterm's `EventStream`. The `ipc_rx` stream wraps the NDJSON codec reading from the Unix socket. Both are async streams, so `select!` multiplexes them naturally.

### Validation Command Execution

Validation commands run in the **target repo root** (not in any worktree). Before validation, the Integrator merges the bundle's branch into the integration branch. The validation commands then check the merged result:

```
target_repo/                    ← validation commands run here
├── .worktrees/
│   └── wi-20260225-a1b2/      ← implementer's worktree (isolated)
└── (main working tree)         ← integrator merges bundle here, then validates
```

If validation passes, the merge commit SHA becomes the Tick's `integration_sha`. If validation fails, the merge is rolled back.

## Alternatives Considered

### Alternative 1: TaskStore-as-Bus (Gas Town Model)

- **Description:** Skip the daemon entirely. TUI and agents read/write TaskStore directly. Communication happens through the data plane (poll for changes or use inotify).
- **Pros:** Massively simpler. No IPC protocol. No daemon process. Crash-resilient (everything in files).
- **Cons:** Multi-writer races. No centralized FSM enforcement. Polling latency. Invalid states can be written. No real-time push.
- **Why not chosen:** Violates "correctness first" principle. Gas Town proved this model leads to chaos. The daemon is the correctness chokepoint.

### Alternative 2: Hybrid (TaskStore for data, thin notify channel)

- **Description:** Both TUI and daemon read/write TaskStore directly. A tiny Unix socket sends "something changed" notifications (no payload).
- **Pros:** Gas Town simplicity + responsive TUI.
- **Cons:** Still has multi-writer problem. FSM enforcement is split between readers.
- **Why not chosen:** Half-measure. Either the daemon is the authority or it isn't.

### Alternative 3: gRPC / Protobuf IPC

- **Description:** Use tonic + prost for structured RPC instead of NDJSON.
- **Pros:** Schema-enforced protocol. Code generation. Performance.
- **Cons:** Heavy dependency. Harder to debug (binary protocol). Overkill for single-machine Unix socket.
- **Why not chosen:** NDJSON is human-readable, debuggable with `socat`, and adequate for our throughput needs.

### Alternative 4: External FSM Crate (statig, rust-fsm)

- **Description:** Use an existing Rust FSM crate instead of hand-rolling.
- **Pros:** Less code to write. Battle-tested state machine semantics.
- **Cons:** No crate fits. `statig` is designed for reactive event loops, not domain records. `rust-fsm` has no guards, no serde, no async. Everything else is dead or stale. Typestate doesn't work with serde round-tripping or mixed-state collections.
- **Why not chosen:** Hand-rolled enum + const transition table is simpler, serde-friendly, auditable, and testable. 50 lines of code vs. fighting a crate's assumptions.

### Alternative 5: Workspace with Multiple Crates

- **Description:** Split into `loopr-core`, `loopr-daemon`, `loopr-tui`, `loopr-cli` crates.
- **Pros:** Clean separation. Independent compilation. Reusable core.
- **Cons:** Premature for MVP1. Single developer. Adds workspace coordination overhead.
- **Why not chosen:** Start with single crate, split later if needed. Module boundaries provide sufficient separation.

## Technical Considerations

### Dependencies

**External crates:**
- `taskstore` — persistence (JSONL + SQLite + Git). Already mature. Git dependency.
- `tokio` (full) — async runtime for daemon
- `ratatui` + `crossterm` — TUI rendering
- `serde` + `serde_json` — serialization
- `clap` (derive) — CLI parsing
- `tokio-util` (codec) — NDJSON framing
- `eyre` — error handling
- `thiserror` — error derives
- `env_logger` + `log` — logging

**Internal:**
- `scottidler/taskstore` — the only external Rust dependency that isn't on crates.io. Pulled via Git.

### Performance

Not a concern for MVP1. Serial execution, single user, local machine. TaskStore's SQLite cache handles query performance. The daemon event broadcast is O(connected clients), which is 1.

### Security

MVP1 is single-user, local-only. Unix socket permissions provide process-level access control. No network exposure. No secrets in TaskStore records.

### Testing Strategy

**Unit tests:**
- Every valid FSM transition succeeds
- Every invalid FSM transition fails with appropriate error
- Per-state invariant enforcement
- Record serialization round-trips (serde)
- ID generation uniqueness
- Transition table exhaustiveness (generate from table, verify no gaps)

**Integration tests:**
- Daemon start/stop lifecycle
- IPC handshake + request/response
- Full CRUD through IPC
- FSM transitions through IPC
- Worktree create/cleanup
- Event broadcast to multiple clients
- End-to-end pipeline: Plan → Spec → Phase → Work → Bundle → Tick

**Property-based (optional):**
- Generate random transition sequences, verify FSM never reaches invalid state
- Graphviz diagram generation from transition tables (visual audit)

### Observability

- `env_logger` with `RUST_LOG` levels (trace/debug/info/warn/error)
- Daemon logs every FSM transition with from/to/role
- Daemon logs every IPC request/response
- TUI status bar shows connection state, current tick, daemon version
- `system.status` IPC method returns daemon uptime, record counts, active worktrees

## Edge Cases and Failure Modes

### Daemon Crash Recovery

On startup, the daemon scans for inconsistent state:

1. **Orphaned worktrees** — List `.worktrees/` entries. For each, check if the associated Work is in InProgress state. If the Work doesn't exist or is in a terminal state (Done, Abandoned), clean up the worktree and delete the branch.
2. **InProgress Works without worktrees** — If a Work is InProgress but its worktree is missing, transition it back to Ready (Coordinator role, logged as recovery action).
3. **Ticks stuck in Sealing/Validating** — Transition back to Open (Integrator role, logged as recovery). The human can re-attempt.
4. **Bundles stuck in Integrating** — Transition back to Accepted (Integrator role, logged as recovery).

### TUI Disconnection

If the daemon goes away while the TUI is running:
- The TUI detects the broken socket (read returns EOF or write returns broken pipe)
- Display a "Disconnected — attempting to reconnect..." banner
- Retry connection every 2 seconds (up to 30 attempts)
- On reconnect, re-fetch all state via `*.list` calls (full resync)
- If reconnect fails after 30 attempts, exit with error message

### Referential Integrity

The daemon validates parent references on record creation:
- `Spec.plan_id` must reference an existing Plan
- `Phase.spec_id` must reference an existing Spec
- `Work.phase_id` must reference an existing Phase
- `Bundle.work_id` must reference an existing Work
- `Work.dependencies` must all reference existing Works

### Record Deletion

MVP1 does **not** support cascading deletes. Deletion rules:
- A record with children (Plan with Specs, Spec with Phases, etc.) **cannot be deleted** — returns an error
- To remove a hierarchy, delete bottom-up (Works first, then Phases, then Specs, then Plan)
- Alternative: transition the parent to Abandoned, which is a terminal state but preserves history
- Lock records can be deleted (released) at any time

### Git Branch Collision on Worktree Create

If `agent/<work_id>` branch already exists (from a previous failed attempt):
1. Check if a worktree is using it (`git worktree list`)
2. If no worktree uses it, delete the branch (`git branch -D agent/<id>`) and retry
3. If a worktree uses it, return an error explaining the conflict

### Multiple TUI Clients

The daemon supports multiple simultaneous clients (each gets its own `tokio::spawn`). All clients receive the same broadcast events. The daemon serializes write access via `RwLock`, so concurrent transitions from different clients are safe — one wins, one gets a stale-state error and should re-fetch.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| TaskStore API doesn't support a needed query pattern | Low | Medium | TaskStore is our own crate; we can extend it. Filter system covers most needs. |
| IPC protocol complexity slows development | Medium | Medium | Start with minimal method catalog. NDJSON is simple to debug. Add methods incrementally. |
| TUI complexity consumes most of the effort | Medium | High | Phase 4 is after the spine works. Can ship CLI-only first and add TUI incrementally. |
| FSM transition tables miss edge cases | Low | High | Exhaustive unit tests. Generate Graphviz diagrams for visual audit. Table is data — easy to review. |
| Worktree management has Git edge cases | Medium | Medium | MVP1 worktrees are simple (create/cleanup). Complex scenarios (rebase, conflict) deferred to MVP3+. |
| Single-crate organization becomes unwieldy | Low | Low | Module boundaries are clean. Extract crates when pain is real, not speculative. |
| Daemon crash loses in-flight state | Medium | Medium | TaskStore writes are durable (JSONL flush). Events are best-effort. TUI re-syncs on reconnect. |
| Git worktree edge cases (detached HEAD, dirty state) | Medium | Low | MVP1 worktrees are simple create/cleanup. Wrap git commands with output parsing and clear error messages. |

## Open Questions

- [x] ~~Should Plan/Spec/Phase have their own simple FSMs?~~ **Resolved:** Yes — shared `HierarchyStatus` enum with `HIERARCHY_TRANSITIONS` table. See "Plan / Spec / Phase Status" section above.
- [ ] What's the exact format for `bundle.claims` and `bundle.verification`? Freeform markdown, or structured fields? (Leaning toward freeform markdown for MVP1, structured for MVP3+.)
- [ ] Should the TUI support multiple simultaneous views (split panes) or strictly one-at-a-time? (Leaning toward one-at-a-time for MVP1 simplicity.)
- [ ] Where does the target repo configuration live — project-level `.loopr/config.toml`, or user-level `~/.config/loopr/config.toml`, or both with merge? (Leaning toward project-level with user-level fallback.)
- [ ] Should `learning.promote` require a minimum reinforcement count (e.g., 3) as discussed in the ChatGPT conversation, or is that MVP3+ policy? (Leaning toward no enforcement in MVP1 — manual promotion by Coordinator.)

## References

- `docs/v3-chatgpt-loopr-architecture-conversation.md` — Full architecture discussion establishing persona model, records, FSMs, strategy knobs
- `docs/v3-claude-loopr-mvp-and-fsm-conversation.md` — MVP scoping, FSM crate evaluation, hand-rolled FSM decision
- `docs/v3-preplan-conversation.md` — Synthesis of all prior work, v2 pattern evaluation, IPC vs TaskStore-as-bus decision
- `docs/v2-proven-patterns.md` — Infrastructure patterns proven in v2 (daemon, IPC, TUI, crash recovery)
- `docs/v2-light-loops-heavy-tools.md` — Light Loops (Tokio tasks) / Heavy Tools (OS subprocesses) architecture
- `docs/yegge/welcome-to-gas-town.md` — Gas Town agent coordination (inspiration, not template)
- `scottidler/taskstore` — TaskStore crate API (Record trait, Store, Filter, JSONL + SQLite)
