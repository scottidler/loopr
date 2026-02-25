# v3 Preplan Conversation

**Date:** 2026-02-25
**Participants:** Scott (User), Claude (Opus 4.6)
**Purpose:** Synthesize all prior design work, evaluate v2 patterns, study Gas Town/Beads, and settle foundational v3 architecture decisions before writing the design doc.

---

## Phase 1: Reading All Prior Docs

### User said:

Read these docs first (general shape of what we have approved so far as well as background):

- `chatgpt-loopr-architecture-conversation.md`
- `claude-loopr-mvp-and-fsm-conversation.md`

Then read these details from previous builds; see what makes sense to bring forward and what does not, architecturally:

- `v2-implementation-status.md`
- `v2-light-loops-heavy-tools.md`
- `v2-proven-patterns.md`

### Claude said:

*(After reading all five documents)*

Here's my synthesis.

---

#### What Loopr Is

A TUI-based "dev team in a box" that orchestrates work through a **Plan → Spec → Phase → Code** hierarchy, using the Ralph Wiggum Loop (fresh LLM context each iteration). TaskStore is the persistence spine. Three FSMs (WorkItem, Bundle, Tick) govern state transitions.

#### Approved Architecture (from the two conversations)

These are settled decisions:

- **TaskStore** as persistence spine (JSONL truth, SQLite cache, Git merge driver) — already a mature external crate
- **Tick** (immutable integration checkpoint, Git SHA) vs **Iteration** (one stateless persona run)
- **Persona model**: Coordinator, Integrator, Implementers, Spec/Review/Research swarms
- **Records**: Plan, Spec, Phase, WorkItem, Bundle, Tick, Learning, Lock
- **FSM**: Hand-rolled enum + const transition table, no crate. Three machines (WorkItem, Bundle, Tick) with role-based guards
- **MVP phasing**: MVP1 = no LLM (prove the spine), MVP2 = LLM as doc validator, MVP3+ = LLM agents
- **Light Loops, Heavy Tools**: Loops are Tokio tasks; Tools are OS subprocesses in worktrees
- **Client fork-to-daemon**: Single binary, auto-starts daemon
- **TUI as thin client**: Pure display + input, all state lives in daemon

#### What to Bring Forward from v2 (Proven Patterns)

These were **actually implemented and working** — carry the patterns (not the code):

| Pattern | Why Keep |
|---------|----------|
| Client fork-to-daemon | Proven, essential UX. Single binary, auto-start, no user ceremony |
| Tokio `select!` main loop | Proven concurrent IPC + signals + tick management |
| NDJSON over Unix socket | Simple, debuggable with `socat`, works |
| Version handshake on connect | Prevents stale daemon mismatches — real bug prevention |
| PID file + `kill(pid, 0)` | Reliable process lifecycle |
| `broadcast` channel for events | Efficient pub/sub to multiple TUI clients |
| `oneshot` channels for req/resp | Clean async correlation |
| `DaemonContext` as shared state hub | Single owner of mutable state, `Arc<RwLock<_>>` for interior mutability |
| TUI as thin client (ratatui) | Correct separation — TUI never touches store directly |
| CLI commands via IPC | Headless operation without TUI — enables scripting |
| SIGTERM → wait → SIGKILL shutdown | Graceful escalation, PID cleanup |
| Crash recovery | Resume interrupted work on restart |

#### What Does NOT Make Sense to Bring Forward

| v2 Thing | Why Drop |
|----------|----------|
| **v2's "Loop" record model** | v2 had one `Loop` type with `LoopType` enum. The ChatGPT conversation established a cleaner hierarchy: distinct Plan/Spec/Phase/WorkItem records. Don't regress. |
| **Runner subprocesses with 3 lanes** | Designed but never actually wired. MVP1 has no LLM and no automated tool execution. When tools are needed (MVP3+), start with simple `Command::new()` before building the full lane system. |
| **LLM client / tool system / prompt templates** | Not needed for MVP1. Build fresh when MVP2/3 arrives — approach may change (Claude Code SDK? Agent SDK?). |
| **Network sandboxing / seccomp** | Way premature. Defer indefinitely. |
| **Invalidation cascade automation** | The *data model* for staleness is right, but automated cascade is MVP3+ territory. MVP1 is manual. |
| **Tool catalog (TOML-based)** | Not needed for MVP1. |
| **Rebase-on-merge protocol** | MVP1 is serial. Complex worktree coordination can wait. |
| **Porting v2 code directly** | v3 is clean slate for good reason. v2 had 65+ files and hit a wall. Carry the *patterns*, write fresh code. |

#### The Key Architectural Tension to Resolve

v2's **DaemonContext** was shaped around its own domain types (Loop, Event, Signal, ToolJob, Outcome). v3 needs to reshape this around the **approved record model**:

```rust
DaemonContext {
    store: TaskStore,          // Plan, Spec, Phase, WorkItem, Bundle, Tick, Learning, Lock
    worktrees: WorktreeManager,
    event_tx: broadcast::Sender<Event>,
    // NO llm_client, tool_router, prompt_engine in MVP1
}
```

The daemon handlers would map to the FSM transitions — `workitem.transition(Ready, Role::Coordinator)` etc. — driven by TUI/CLI commands via IPC.

#### Bottom Line

**v2 proved the infrastructure** (daemon, IPC, TUI, signals, crash recovery). **The ChatGPT conversation proved the domain model** (records, FSMs, tick semantics, MVP phasing). v3 should marry the two: v2's proven infrastructure patterns with the ChatGPT conversation's cleaner domain architecture, starting with MVP1 (no LLM, human-as-coordinator).

---

## Phase 2: Is IPC Still Relevant?

### User said:

So the one question I had was is IPC still relevant? In Steve Yegge's gastown+beads, the message passing was through the jsonl+sqlite3, correct? You can find 3 docs about gastown+beads in `docs/yegge/`.

### Claude said:

*(After reading all three Yegge docs: "Welcome to Gas Town", "The Future of Coding Agents", "Gas Town Emergency User Manual")*

#### How Gas Town Communicates (No IPC)

Gas Town has **zero custom IPC**. The communication architecture is:

1. **Beads (JSONL + SQLite + Git) IS the message bus.** Mail, hooks, work items, agent identities, patrol state — everything is a Bead. Workers read/write Beads. That's it.

2. **`gt` / `bd` CLI commands** are just convenience wrappers over Beads read/write operations. `gt sling` = write a hook bead. `gt nudge` = tmux `send-keys` to poke an agent. `bd create` = create a bead record.

3. **tmux for real-time** — the only "real-time" messaging is literally typing into a terminal via `tmux send-keys`. The GUPP nudge is a tmux keystroke.

4. **No daemon in the traditional sense** — the Deacon patrols by polling Beads on a timer. It's just another Claude Code session reading the data plane.

So in Gas Town, **the data store IS the communication layer**. There is no socket, no protocol, no codec. Agents are loosely coupled through the file system.

#### What This Means for Loopr

The v2 IPC stack (Unix socket, NDJSON, version handshake, request/response correlation, broadcast channels) was built to solve a problem that Gas Town solves differently. The question is: **which approach fits Loopr's architecture better?**

The key difference is where loops execute:

| | Gas Town | Loopr |
|---|---|---|
| **Agents** | Separate Claude Code OS processes | Tokio tasks inside a daemon |
| **Communication** | Read/write Beads (file-based) | v2: IPC over Unix socket |
| **UI** | tmux sessions | ratatui TUI |
| **Coordination** | Patrol agents poll Beads | v2: daemon dispatches |

Gas Town can skip IPC because each agent is a **separate process with its own Claude Code session**. They don't need to talk to each other in real-time — they read the shared data plane at their own pace.

Loopr's "Light Loops, Heavy Tools" architecture puts loops **inside** a single daemon process as Tokio tasks. The TUI is a separate process. So you need *something* between TUI and daemon.

There's a spectrum of options:

**Option A: Full IPC (v2 model)**
- Daemon owns all state
- TUI is 100% thin client
- All commands go TUI → socket → daemon
- All updates go daemon → socket → TUI
- Pros: real-time push, clean separation
- Cons: protocol complexity, codec, version handshake, connection management

**Option B: TaskStore-as-bus (Gas Town model)**
- TaskStore is the shared data plane
- Daemon reads TaskStore for work, writes results back
- TUI reads TaskStore for display, writes commands as records
- Daemon watches for new command records (poll or inotify)
- Pros: massively simpler, crash-resilient, everything in Git, no protocol
- Cons: polling latency for TUI updates, no streaming

**Option C: Hybrid — TaskStore for coordination, thin notify channel**
- TaskStore is the data plane (like Beads)
- Daemon and TUI both read/write TaskStore directly
- A tiny Unix socket or pipe just sends "something changed" notifications (no payload)
- TUI gets a poke, re-reads TaskStore, re-renders
- Pros: Gas Town simplicity + responsive TUI
- Cons: still need a notification mechanism, but it's trivial compared to full IPC

Initial recommendation was Option B for MVP1, Option C for MVP2+, with no daemon at all in MVP1 — the TUI would be a direct TaskStore CRUD interface.

---

## Phase 3: Why Gas Town's Model Is Wrong for Loopr

### User said:

One thing to keep in mind, is we are not trying to duplicate gastown+beads. It is inspiration. But we are trying to improve it and not make something that is so chaotic and dangerous. Still think we should go your suggested route?

### Claude said:

You're right. I overcorrected toward Gas Town's model without accounting for *why* Gas Town is chaotic.

Gas Town's chaos comes directly from its coordination model:

- Multiple agents write Beads concurrently → race conditions → "the Murder Mystery" (Deacon killing workers because of stale state)
- No centralized authority validates transitions → invalid states sneak in → "heresies"
- Polling for changes → stale data → wrong decisions → work done 2-3 times, work lost
- "Nondeterministic idempotence" is literally an admission that things break and you just redo them

Loopr's #1 principle is **correctness first, speed second**. The FSM transition tables exist specifically to **prevent invalid state changes**. Role-based guards exist to enforce who can do what. Tick semantics exist to provide deterministic integration checkpoints.

All of that falls apart if you let multiple writers race on files.

#### The Daemon IS the Correctness Guarantee

The v2 daemon/IPC model gives you exactly what Gas Town lacks:

- **Single-writer semantics** — the daemon validates every transition before it's committed. No races.
- **FSM enforcement at a chokepoint** — every state change goes through `WorkItem::transition()` inside one process. Invalid transitions are rejected, not silently written.
- **Immediate feedback** — "your transition was rejected because Bundle must be in Reviewed state before Accepted" vs. "I wrote a file and hope it's valid."
- **Real-time push** — TUI shows accurate state, not whatever it last polled.
- **Centralized coordination** — one process is the authority. Not N agents fighting over who writes last.

#### Revised Recommendation: Keep IPC, Keep the Daemon

The daemon + IPC is **not** just plumbing from v2 to carry forward out of habit. It's the architectural enforcement mechanism for the correctness guarantees that differentiate Loopr from Gas Town.

```
TUI (thin client) ──IPC──▶ Daemon (single authority)
                                │
                                ├── validates FSM transitions
                                ├── enforces role guards
                                ├── manages worktrees
                                ├── writes to TaskStore
                                └── (MVP2+) runs loops as Tokio tasks
```

TaskStore remains the persistence spine — JSONL truth, SQLite cache, Git merge driver. But it's the **storage layer**, not the communication bus. The daemon mediates all access to it.

The full v2 IPC stack (NDJSON over Unix socket, version handshake, broadcast events, request/response correlation) earns its keep because it provides the structured, validated command surface that makes "correctness first" real rather than aspirational.

---

## Summary of Settled Decisions

### From Prior Conversations (Unchanged)

1. Rust + ratatui
2. TaskStore as persistence spine (JSONL truth, SQLite cache, Git merge driver)
3. Records: Plan, Spec, Phase, WorkItem, Bundle, Tick, Learning, Lock
4. Three FSMs: WorkItem, Bundle, Tick — hand-rolled enum + const transition table with role-based guards
5. MVP1 = no LLM, human-as-coordinator, prove the spine
6. MVP2 = LLM as doc validator (read-only, safest entry point)
7. MVP3+ = LLM implementers + reviewers
8. Light Loops (Tokio tasks), Heavy Tools (OS subprocesses in worktrees)
9. Correctness first, speed second

### From This Conversation (New)

10. **Keep the daemon + IPC architecture from v2** — the daemon is the single authority that validates all FSM transitions and enforces role guards. This is what prevents Gas Town-style chaos.
11. **TaskStore is storage, not communication** — unlike Gas Town where Beads is both store and message bus, Loopr keeps these concerns separate. The daemon mediates all access to TaskStore.
12. **Carry v2 patterns, not v2 code** — v3 is a clean slate. The patterns (client fork-to-daemon, NDJSON over Unix socket, version handshake, broadcast events, DaemonContext, thin TUI client, CLI via IPC, crash recovery) are proven and should be reimplemented fresh against the new domain model.
13. **Gas Town/Beads is inspiration, not a template** — we learn from the MEOW stack, molecular workflows, GUPP, and the data-plane-as-truth principle, but explicitly reject the multi-writer file-based coordination that causes Gas Town's chaos.

### Open for Design Doc

- Crate layout / workspace structure
- Exact TaskStore record schemas
- TUI view specifications
- Worktree lifecycle management details
- Configuration format and defaults for strategy knobs
- Observability / logging architecture
- Error handling patterns
- IPC method catalog (which commands the daemon exposes)
