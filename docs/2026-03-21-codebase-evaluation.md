# Loopr v3 - Codebase Evaluation

## What It Is

Loopr is a **68,000-line Rust TUI application** that orchestrates autonomous LLM-powered software development agents. It's a single binary (`loopr`) built on a daemon/client architecture with 105 source files, 410 transitive dependencies, and 614 commits across 14 tagged releases (currently v0.1.13).

### Architecture at a Glance

```
┌─────────────────────────────────────────────────────┐
│  TUI (ratatui)                                      │
│  8 views: Dashboard, Chat, Agents, Works, Bundles,  │
│           Ticks, Learnings, Locks                    │
└──────────────────────┬──────────────────────────────┘
                       │ Unix socket (JSONL IPC)
┌──────────────────────▼──────────────────────────────┐
│  Daemon (single-writer authority)                    │
│  ┌─────────────┐  ┌──────────────┐  ┌────────────┐ │
│  │ FSM Engine   │  │ TaskStore    │  │ Work Queue │ │
│  │ (transitions │  │ (JSONL+SQLite│  │ (pull-based│ │
│  │  + guards)   │  │  persistence)│  │  assign)   │ │
│  └─────────────┘  └──────────────┘  └────────────┘ │
│  ┌─────────────────────────────────────────────────┐ │
│  │ Agent Supervisor                                 │ │
│  │  Coordinator ─ Researcher ─ Implementer          │ │
│  │  Reviewer ─ Integrator ─ Executor                │ │
│  └─────────────────────────────────────────────────┘ │
│  ┌─────────────────────────────────────────────────┐ │
│  │ Tool System (14 builtins + configured)           │ │
│  │  read, write, edit, glob, grep, find, list,      │ │
│  │  tree, shell, fetch, search, delegate, plan, todo│ │
│  └─────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────┘
```

### Core Subsystems

**1. Daemon (16,110 LOC)** - Double-fork Unix daemon with version-checked lifecycle. Single writer enforcing all state transitions. Handlers.rs alone is 13,331 lines - the beating heart of the system.

**2. Agents (20,993 LOC)** - Six agent roles:
- **Coordinator** - Long-lived meta-agent operating across Plan/Spec/Phase/Work levels. Adaptive timer (5s active, 30s idle). Drives the entire decomposition and execution lifecycle.
- **Implementer** - Executes work items in isolated git worktrees using the Ralph Wiggum Loop (fresh context each iteration, no conversation carryover).
- **Reviewer** - Single-pass code review producing structured verdicts (Approve/RequestChanges/Reject).
- **Researcher** - Codebase search (ripgrep, glob, file reads) producing Learnings.
- **Integrator** - Fully deterministic (no LLM). Pure FSM for Tick lifecycle: find Accepted bundles, seal, validate, publish/fail.
- **Executor** - Action dispatch layer translating agent decisions into IPC calls.

**3. Domain Model (5,705 LOC)** - Hierarchical decomposition with four FSMs:
- `Plan → Spec → Phase → Work` (hierarchy, each with Draft/Active/Complete/Abandoned)
- `Work` (Draft → Ready → InProgress → Blocked → InReview → Integrated → Done)
- `Bundle` (Proposed → Triaged → Reviewed → Accepted → Integrating → Merged/Rejected)
- `Tick` (Draft → Active → Failed → Published)

**4. Tool System (4,596 LOC)** - Unified `Tool` trait with 14 builtins. Agentic loop with streaming SSE, parallel tool execution, two-tier context management (microcompact at 4KB + LLM summarization at 150K tokens), and a `delegate` tool that spawns isolated subagent loops.

**5. TUI (5,329 LOC)** - Ratatui-based with 8 tabbed views, chat interface with vim keybindings, SSE batch rendering at ~30fps, and a chat funnel state machine (Chat → Interview → PlanDraft → Executing).

**6. IPC (1,629 LOC)** - JSON-RPC over Unix sockets with structured error codes (-32000 through -32005 for domain errors like transition_rejected, stale_bundle, validation_required, pool_exhausted).

**7. Prompts (21 .pmt files)** - Compiled into the binary via `include_str!()` with filesystem override capability. Per-agent, per-phase, per-funnel-state prompt selection.

**8. Persistence** - TaskStore (external crate `scottidler/taskstore`) provides JSONL-as-truth with SQLite-as-cache. In-memory shadow stores (HashMap behind StdRwLock) for hot-path reads.

### Implementation Maturity

| Layer | Status | Description |
|-------|--------|-------------|
| Orchestration Spine | Implemented | Daemon, FSMs, TaskStore, IPC, worktrees |
| Doc Validator | Implemented | Sync LLM validation gate (ureq + Anthropic) |
| Code-Level Agents | Implemented | Implementer + Reviewer with tool execution |
| Full Agent Roster | Implemented | Coordinator, Researcher, Integrator added |
| Pipeline Hardening | Implemented | 23 defects fixed across 4 phases |
| Chat + Agentic Tools | Implemented | 14 builtins, streaming, delegation, funnel |
| Semantic Decomposition | Partial | Coverage evaluator done; bubble-up/interview incomplete |

### Test Coverage

94.3% of production files have `#[cfg(test)]` modules. The only untested files are module aggregators (`mod.rs`) and `main.rs`. Two dedicated test files add 5,820 lines of FSM correctness and integration tests.

---

## What It's Trying to Be

Loopr aspires to be a **"dev team in a box"** - a system where a user describes what they want built, and autonomous agents decompose, implement, review, and integrate the work with minimal human intervention.

### The Vision

The user opens the TUI, types a high-level goal into the chat interface, and the system:

1. **Interviews** - Asks clarifying questions to sharpen intent
2. **Plans** - Generates a Plan document, validated by LLM gates
3. **Decomposes** - Breaks the Plan into Specs → Phases → Work items, with coverage evaluation ensuring nothing falls through the cracks
4. **Executes** - Assigns Work to Implementer agents running in isolated worktrees, each doing fresh-context iterations (Ralph Wiggum Loop)
5. **Reviews** - Reviewer agents check code quality, producing structured feedback
6. **Integrates** - Deterministic Integrator seals Ticks (integration checkpoints), validates, and publishes
7. **Learns** - Learnings accumulate across iterations, with confidence scoring and role-applicability tags

The key architectural bet is **correctness first, intelligence second**. The entire spine was proven to work without LLMs before any AI was plugged in. Intelligence is layered in: first read-only (validation), then code-writing (agents), then orchestration-level (Coordinator).

### Design Principles

- **Single-writer daemon** prevents all race conditions and data corruption
- **Fresh-context iterations** (RWL) enable unbounded work without context window limits
- **TaskStore persistence** gives crash recovery and full audit trails
- **Deterministic integration** (no LLM in the Integrator) means the merge pipeline can't hallucinate
- **Advisory locks** make file contention visible across parallel agents

### Current Gap: The Bridge

The active development front is the **chat-to-orchestration bridge** - connecting the conversational chat interface to the autonomous execution pipeline. The funnel states exist (Chat → Interview → PlanDraft → Executing), the `/accept` command creates a CoordinatorGoal, but the full end-to-end flow from "user types a request" to "agents deliver integrated code" is still being wired.

The semantic decomposition layer (coverage evaluation ensuring parent→children completeness, upward feedback when children fail) is partially implemented - the evaluator exists but the bubble-up logic and interview FSM wiring are incomplete.

### Incomplete Design Aspirations

From the design docs:
- **File-touch broadcasting** (Draft, not started) - auto-lock on file writes, broadcast to other agents
- **Collaborative Plan interview** - IPC handlers for interactive refinement before autonomous execution
- **Coverage gates in Coordinator loop** - automatic re-decomposition when coverage evaluation fails
- **Work dependency cycle detection** - BFS/DFS validation of `depends_on` fields

---

## Honest Assessment

### Strengths

- **Architecturally sound** - The daemon/FSM/TaskStore spine is well-designed and battle-tested through 23 hardening fixes. The single-writer model eliminates an entire class of concurrency bugs.
- **Deeply thought through** - 60+ design documents show serious architectural consideration. Layers are orthogonal and opt-in.
- **Strong test discipline** - 94.3% coverage with dedicated FSM correctness tests.
- **Robust LLM integration** - The agentic loop handles streaming, parallel tool execution, context compaction, and loop detection (Lifeguard). Action parsing tolerates LLM deviations (markdown fences, key normalization, prose tolerance).
- **Good separation of concerns** - Domain types, IPC protocol, daemon handlers, agent logic, and TUI rendering are cleanly separated.

### Concerns

- **2,047 `.unwrap()` calls in production code** - This is the single largest code quality issue, directly violating the project's own stated guideline. Given `#![deny(clippy::unwrap_used)]` is set in lib.rs, these are likely concentrated in binary-side code or gated behind cfg attributes, but the volume is concerning.
- **handlers.rs is 13,331 lines** - This monolith file handles all RPC dispatch and is a maintenance risk. It would benefit from decomposition.
- **12 `_variable` pattern violations** - Minor but consistent with the project's own conventions being aspirational rather than enforced.
- **No integration test directory** - All tests are in-module. No end-to-end tests that spin up a real daemon and exercise the chat-to-execution pipeline.
- **Prompt type safety** - String `.replace()` for template placeholders with no compile-time verification that all placeholders are filled.
- **Design doc vs. reality gap** - Some design docs reference features that may not be fully wired (semantic decomposition bubble-up, file-touch broadcasting). The 60+ design docs represent significant design debt if they drift from implementation.

### Bottom Line

Loopr is a **serious, well-architected orchestration system** that has invested heavily in correctness guarantees (FSMs, single-writer, deterministic integration) before layering in LLM intelligence. The core spine is solid and the agent/tool system is sophisticated. The main risk is complexity - at 68K lines with a single developer, the system's ambition exceeds what's easily maintainable. The current focus on bridging chat-to-orchestration is the right priority, as that's the gap between "working subsystems" and "working product."
