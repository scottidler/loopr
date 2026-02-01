# Loopr v2: Design Documentation

**Version:** 2.0
**Date:** 2026-01-31
**Status:** Implementation Spec

---

## What is Loopr?

Loopr is an autonomous coding agent that executes complex software engineering tasks through hierarchical loops. Users describe what they want in natural language, and Loopr:

1. Creates a **Plan** (high-level approach)
2. Decomposes into **Specs** (detailed requirements)
3. Breaks specs into **Phases** (incremental steps)
4. Executes phases via **Code loops** (the actual coding work)

Each level produces artifacts (plan.md, spec.md, phase.md) that define what child loops must accomplish. This creates a self-correcting system that can handle multi-hour tasks autonomously.

---

## Process Model

```
┌─────────────────────────────────────────────────────────────────────────┐
│                              User                                        │
└─────────────────────────────────────────────────────────────────────────┘
                                   │
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                           TUI Client                                     │
│  - Interactive frontend (ratatui)                                        │
│  - Chat view + Loops view                                                │
│  - Connects to daemon via Unix socket                                    │
│  - Can detach/reattach without stopping loops                            │
└─────────────────────────────────────────────────────────────────────────┘
                                   │ IPC (Unix socket)
                                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                             Daemon                                       │
│  - Long-running orchestrator (Tokio async)                               │
│  - LoopManager: schedules and runs loops                                 │
│  - LlmClient: Anthropic API calls                                        │
│  - ToolRouter: routes tool calls to runners                              │
│  - Reads/writes TaskStore for all persistent state                       │
└─────────────────────────────────────────────────────────────────────────┘
           │                    │                    │
           ▼                    ▼                    ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│  runner-no-net  │  │   runner-net    │  │  runner-heavy   │
│  (sandboxed)    │  │   (default)     │  │  (builds/tests) │
│                 │  │                 │  │                 │
│ - No network    │  │ - Network OK    │  │ - Low concurrency│
│ - File I/O only │  │ - Web access    │  │ - Long timeouts  │
└─────────────────┘  └─────────────────┘  └─────────────────┘
```

---

## The Core Concept: The Loop

> **Read [loop.md](loop.md) first. It is the essential document.**

Loopr implements the **Ralph Wiggum pattern** - an iterative loop that calls an LLM with fresh context on each iteration until validation passes.

```bash
# The original Ralph Wiggum (bash)
while :; do cat PROMPT.md | claude ; done
```

We productionalize this:
- **Tokio tasks** instead of OS processes (efficient, ~2MB not ~200MB)
- **Async HTTP** to Anthropic instead of spawning `claude` CLI
- **Fresh context** = new `messages` array each API call (no conversation history)
- **Feedback in prompt** = accumulated errors injected into prompt text

**We are NOT Gas Town.** We don't spawn hundreds of processes. We run lightweight async tasks making HTTP calls.

---

## The Four Loop Types

| Type | Produces | Validation | Spawns |
|------|----------|------------|--------|
| **Plan** | plan.md | Format + LLM-as-Judge + **User approval** | Specs |
| **Spec** | spec.md | Format + LLM-as-Judge | Phases |
| **Phase** | phase.md | Format + LLM-as-Judge | Code |
| **Code** | code/docs | `cargo test`, `otto ci` | Nothing |

**Each loop has ONE job:** produce its artifact(s). What happens downstream is not its concern.

**Artifacts are first-class outputs** - plan.md, spec.md, phase.md are versioned alongside code.

---

## The Hierarchy (The Onion)

```
User: "Add OAuth authentication"
         │
         ▼
    PlanLoop ──────────────────────────────────────┐
      │ iterates until plan.md validates           │
      │                                            │
      │ [USER APPROVES]                            │
      ▼                                            │
    SpecLoop (×N) ─────────────────────────┐       │
      │ iterates until spec.md validates   │       │
      ▼                                    │       │
    PhaseLoop (×N) ────────────────┐       │       │
      │ iterates until phase.md    │       │       │
      ▼                            │       │       │
    CodeLoop ──────────────┐       │       │       │
      │ iterates until     │       │       │       │
      │ tests pass         │       │       │       │
      └────────────────────┘       │       │       │
                                   │       │       │
    [All complete → merge to main] ◄───────┴───────┘
```

Nothing merges until the entire hierarchy completes successfully.

---

## Architecture: Daemon + Runners

```
┌─────────────────────────────────────────────────────────────────┐
│                           Daemon                                 │
│                                                                  │
│   LoopManager                                                   │
│   ├── Loop (tokio task) ───async HTTP──→ Anthropic API         │
│   ├── Loop (tokio task) ───async HTTP──→ Anthropic API         │
│   ├── Loop (tokio task) ───async HTTP──→ Anthropic API         │
│   └── ... (50+ concurrent, ~2MB each)                          │
│              │                                                   │
│              │ IPC (tool calls)                                  │
│              ▼                                                   │
│   ┌─────────────────────────────────────────────────┐           │
│   │ Runners (subprocesses)                          │           │
│   │ ├── runner-no-net (10 slots, sandboxed)        │           │
│   │ ├── runner-net (5 slots, network allowed)      │           │
│   │ └── runner-heavy (1 slot, builds/tests)        │           │
│   └─────────────────────────────────────────────────┘           │
└─────────────────────────────────────────────────────────────────┘
         │
         │ IPC (Unix socket)
         ▼
┌─────────────────────────────────────────────────────────────────┐
│                         TUI Client                               │
│   - Can detach/reattach without stopping loops                  │
│   - Chat view + Loops view                                      │
└─────────────────────────────────────────────────────────────────┘
```

---

## Key Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Runtime model | Tokio async tasks | Efficient (~2MB), not Gas Town (~200MB) |
| Fresh context | New messages array each iteration | Prevents context rot |
| Process model | Daemon + TUI + Runners | TUI can detach, tools are sandboxed |
| Coordination | TaskStore polling | Survives crashes, full audit trail |
| Tool routing | Runner lanes | Network isolation, resource control |
| Loop state | JSONL + SQLite | Git-friendly, fast queries |
| Workspace | Git worktrees | Parallel work without conflicts |
| Artifacts | First-class outputs | plan.md/spec.md/phase.md are versioned |
| User gate | Plan approval only | Everything else is autonomous |

---

## Documentation Index

### Implementation (Start Here)

| Document | Description |
|----------|-------------|
| **[implementation-phases.md](implementation-phases.md)** | **BUILD GUIDE.** 16 phases with code, files, validation. |

### Core (Read First)

| Document | Description |
|----------|-------------|
| **[loop.md](loop.md)** | **THE essential document.** Loop struct, iteration model, hierarchy. |
| [domain-types.md](domain-types.md) | Loop, Signal, ToolJob, Event records |

### Architecture

| Document | Description |
|----------|-------------|
| [architecture.md](architecture.md) | System overview, process relationships |
| [process-model.md](process-model.md) | TUI, Daemon, Runner lifecycle |
| [runners.md](runners.md) | Runner lanes, sandboxing, tool routing |
| [ipc-protocol.md](ipc-protocol.md) | Message schemas between processes |

### Loop System

| Document | Description |
|----------|-------------|
| [loop-architecture.md](loop-architecture.md) | Loop hierarchy, artifacts, invalidation |
| [loop-coordination.md](loop-coordination.md) | Polling-based coordination, signals |
| [worktree-coordination.md](worktree-coordination.md) | Rebase-on-merge protocol for parallel worktrees |
| [scheduler.md](scheduler.md) | Priority model, dependency resolution |
| [execution-model.md](execution-model.md) | Worktree lifecycle, crash recovery |

### Data & Storage

| Document | Description |
|----------|-------------|
| [persistence.md](persistence.md) | TaskStore collections, storage layout |
| [observability.md](observability.md) | Event system, logging, metrics |
| [configuration-reference.md](configuration-reference.md) | All configuration options |

### LLM Integration

| Document | Description |
|----------|-------------|
| [llm-client.md](llm-client.md) | Anthropic client, streaming, tokens |
| [tools.md](tools.md) | Tool definitions with runner assignments |
| [tool-catalog.md](tool-catalog.md) | Canonical tool list (catalog.toml) |
| [artifact-tools.md](artifact-tools.md) | Structured artifact creation via tool_use |

### Validation & Quality

| Document | Description |
|----------|-------------|
| [loop-validation.md](loop-validation.md) | Validation per loop type |
| [rule-of-five.md](rule-of-five.md) | Plan review methodology (optional) |

### User Interface

| Document | Description |
|----------|-------------|
| [tui.md](tui.md) | Chat + Loops views, keyboard nav |

### Reference

| Document | Description |
|----------|-------------|
| [implementation-patterns.md](implementation-patterns.md) | Patterns from taskdaemon |
| [conflicts.md](conflicts.md) | v1→v2 decisions and resolutions |
| [glossary.md](glossary.md) | Term definitions |

---

## Glossary

| Term | Definition |
|------|------------|
| **Loop** | The core abstraction. Iterates with fresh context until validation passes. |
| **Iteration** | One attempt within a loop (prompt → LLM → tools → validate) |
| **Artifact** | Output file (plan.md, spec.md, phase.md) that spawns children |
| **Fresh context** | New `messages` array each API call (no conversation history) |
| **Progress** | Accumulated feedback from failed iterations (in prompt, not messages) |
| **TaskStore** | Persistence layer (JSONL + SQLite) |
| **Worktree** | Git worktree for isolated loop execution |
| **Runner** | Subprocess that executes tools in isolation |
| **Lane** | Category of runner (no-net, net, heavy) |
| **Daemon** | Long-running orchestrator process |

---

## Getting Started

### Prerequisites

```bash
# Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Required environment variable
export ANTHROPIC_API_KEY="your-api-key"
```

### Installation

```bash
git clone https://github.com/scottidler/loopr
cd loopr
cargo build --release
export PATH="$PATH:$(pwd)/target/release"
```

### Usage

```bash
# Start daemon (runs in background)
loopr daemon start

# Launch TUI (connects to daemon)
loopr

# Or start daemon + TUI together
loopr --start-daemon
```

### Basic Workflow

1. **Chat View** - Describe your task
2. **Create Plan** - `/plan Add user authentication`
3. **Approve Plan** - Review plan.md, approve to continue
4. **Monitor Progress** - `Tab` to Loops view, watch hierarchy execute
5. **Review Results** - Code committed to git when complete

---

## References

- [Ralph Wiggum Technique](https://ghuntley.com/ralph/) - Geoffrey Huntley (original concept)
- [Gas Town](https://steve-yegge.medium.com/welcome-to-gas-town-4f25ee16dd04) - Steve Yegge (what we're NOT doing)
- [Anthropic API Docs](https://docs.anthropic.com/)
- [ratatui](https://ratatui.rs/) - TUI framework
