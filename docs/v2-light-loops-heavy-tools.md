# Light Loops, Heavy Tools

**Author:** Scott A. Idler
**Date:** 2026-02-25
**Status:** Design Principle

---

## The Core Insight

Loopr has two fundamentally different kinds of work, and they demand fundamentally different runtimes:

1. **Loops (agents)** — decide what to do. They call an LLM, read the response, and pick the next action. This is I/O-bound waiting on HTTP. It costs almost nothing to have many in flight.

2. **Tools** — do the actual work. They read files, write code, run builds, execute tests. This is real compute and real filesystem mutation that needs isolation, sandboxing, and killability.

Conflating the two is the central mistake of "Gas Town" architectures that spawn a full OS process per agent. Loopr keeps them separate.

---

## Light: Loops as Tokio Tasks

A loop is just an async function on the tokio runtime. It:

- Builds a `messages` array (fresh each iteration — no conversation history)
- Makes an async HTTP call to the Anthropic API
- Parses the response for tool calls or completion
- Repeats until validation passes or max iterations hit

**Cost:** ~2MB of memory per loop. The daemon can run 50+ concurrently in a single process.

```
Daemon Process
    │
    └── LoopManager
            ├── Loop (tokio task) ──async──→ Anthropic API
            ├── Loop (tokio task) ──async──→ Anthropic API
            ├── Loop (tokio task) ──async──→ Anthropic API
            └── ... (50+ concurrent, ~2MB each)
```

There is no process spawn, no fork, no container. A loop is a state machine that suspends at `.await` points while waiting for the network. Tokio multiplexes thousands of these onto a small thread pool.

---

## Heavy: Tools as Runner Subprocesses in Worktrees

When a loop decides to use a tool, the daemon routes that call to a **runner** — a separate OS subprocess that executes the tool inside the loop's **git worktree**.

Each loop gets its own worktree (a cheap git checkout on a feature branch), giving it an isolated copy of the repo. Tools run inside that worktree via runners organized into three lanes:

| Lane | Network | Slots | Timeout | Examples |
|------|---------|-------|---------|----------|
| `local` | Blocked | 10 | 30s | read_file, write_file, grep, glob |
| `net` | Allowed | 5 | 60s | web_fetch, api_call |
| `heavy` | Allowed | 1 | 10min | cargo build, npm test, otto ci |

Runners are real OS processes because tools need things that tokio tasks cannot provide:

- **Process groups** — a `cargo build` spawns rustc, linker, proc-macros. If it times out or the loop is cancelled, we `killpg()` the entire tree. You cannot do this to a tokio task.
- **Sandboxing** — `runner-local` uses network namespaces or seccomp to block all network access. This is a per-process kernel mechanism.
- **Isolation** — each tool execution is scoped to a worktree path. The runner validates that all file operations stay within the sandbox.
- **Resource limits** — output size caps, timeouts, and slot-based concurrency prevent any single tool from starving the system.

```
Daemon
    │
    └── ToolRouter ──Unix socket──→ runner-local (10 slots, sandboxed)
                   ──Unix socket──→ runner-net    (5 slots, network ok)
                   ──Unix socket──→ runner-heavy  (1 slot, builds/tests)
```

---

## Why Not Just One or the Other?

**All-heavy (Gas Town):** Spawn an OS process per agent. Each process is ~200MB. 50 agents = 10GB just for the orchestration layer, before any real work happens. Process creation is slow. IPC between agents is painful. You pay the cost of isolation even for work that doesn't need it (LLM API calls).

**All-light (everything in-process):** Run tools as async functions inside the daemon. Now you can't kill a runaway build without killing the whole daemon. You can't sandbox network access. A tool that blocks or panics takes down all 50 loops. You've traded isolation for convenience and it will bite you.

**Loopr's split:** Loops are light because they're just waiting on HTTP. Tools are heavy because they do real work that needs real isolation. The boundary between them is the ToolRouter, which serializes tool calls over Unix sockets to runner subprocesses.

---

## The Worktree Connection

Git worktrees are what make parallel tool execution possible without file conflicts. Each loop gets a worktree:

```
~/.loopr/worktrees/
    ├── loop-001/    ← branch: loop-001, worktree for Plan loop
    ├── loop-002/    ← branch: loop-002, worktree for Spec loop
    ├── loop-003/    ← branch: loop-003, worktree for Code loop
    └── ...
```

When a tool runs, its `cwd` is the loop's worktree. When a loop completes, its branch is merged and the worktree is removed. This is cheap — git worktrees share the object store, so creating one is just a checkout, not a clone.

Worktrees belong to the "heavy" side of the split. They exist for tools, not for loops. A loop doesn't need a filesystem — it just needs a prompt and an API endpoint. The worktree is where the loop's *tools* do their work.

---

## Summary

| | Loops (Light) | Tools (Heavy) |
|---|---|---|
| **What** | LLM agent iterations | File I/O, builds, tests |
| **Runtime** | Tokio async tasks | OS subprocesses (runners) |
| **Memory** | ~2MB each | Varies (builds can be large) |
| **Concurrency** | 50+ in one process | Slot-limited per lane (10/5/1) |
| **Isolation** | None needed (stateless HTTP) | Worktrees + sandboxing + process groups |
| **Kill mechanism** | Drop the future | `killpg()` on process group |
| **Scaling cost** | Negligible | Real (worktree + process + I/O) |

The principle: **use the cheapest runtime that provides the isolation guarantees you need.** Loops need none, so they're tokio tasks. Tools need real isolation, so they're subprocesses in worktrees.

---

## References

Prior art in earlier branches:

- **v2** `docs/loop.md` — "We are NOT Gas Town" framing, tokio task comparison table, concurrency model diagram
- **v2** `docs/process-model.md` — three-tier process model (TUI + Daemon + Runners), runner architecture, process group kill patterns
- **v2** `docs/runners.md` — runner lane details, tool-to-lane mapping, sandboxing options (network namespaces, seccomp, iptables)
- **v2** `docs/execution-model.md` — worktree lifecycle (creation, tool execution, cleanup), crash recovery
- **v2** `docs/architecture.md` — system overview diagram, component responsibilities
- **v2** `docs/README.md` — "We are NOT Gas Town" summary, runner-heavy lane in architecture diagram
- **v1** `docs/execution-model.md` — original worktree management design (loops with worktrees, pre-runner-subprocess model)
- **v1** `docs/tools.md` — ToolContext scoped to worktrees, sandbox enforcement
- **v1** `docs/scheduler.md` — loop scheduling with tokio
