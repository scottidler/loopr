# Loopr v3 - Claude Code Instructions

## Rules

@.claude/rules/rust.md

## 🚨 CRITICAL WARNINGS 🚨
- **DO NOT install git hooks (like `post-checkout`, `post-merge`, etc.) in the Loopr orchestrator repository itself.** Hooks like "Syncing database from JSONL files..." are meant exclusively for the TARGET repo being orchestrated. Conflating the Loopr codebase with the target repo is a fatal error.

## Project

Loopr is a TUI-based "dev team in a box" orchestrator. The orchestration spine, persistence, all agent roles, chat with agentic tool loop, and streaming are implemented. Current focus: wiring the chat-to-orchestration bridge (chat funnel -> Plan creation -> autonomous execution) and getting a real end-to-end build working.

## Design Docs

Design docs live in `docs/design/` using `YYYY-MM-DD-feature-name.md` naming. Key references for the current architecture:
- `docs/design/2026-02-25-orchestration-spine.md` - daemon, FSMs, TaskStore, IPC, worktrees
- `docs/design/2026-02-26-multi-level-rwl.md` - Coordinator, Researcher, Integrator, context builder
- `docs/design/2026-03-04-native-tool-use.md` - unified Tool trait and builtin tools
- `docs/design/2026-03-05-chat-agentic-tool-loop.md` - chat with agentic loop
- `docs/design/2026-03-03-semantic-decomposition.md` - coverage evaluator, upward feedback (Draft - partial)
- `docs/design/2026-03-01-file-touch-broadcasting.md` - file-touch advisory locks (Draft - not started)

Do not deviate from design docs without discussion.

## Build & Validate

```bash
otto ci          # Full pipeline: lint + check (compile, clippy, fmt) + test
otto test        # Tests only (with ensure-tests-exist gate)
otto check       # Compile + clippy + fmt only
cargo check      # Quick compile check
```

## Conventions

- Async everywhere (tokio)
- thiserror for error types, eyre/anyhow for propagation
- `cargo add` for dependencies - never hand-write versions
- Tests in every module (`#[cfg(test)] mod tests { ... }`)
- No `#[allow(dead_code)]` in final code
- No underscore-prefixed unused variables in final code
- Commit messages: `feat(scope): description` with phase context
- No `async-trait` crate - use native `async fn` / `impl Future` for non-dyn traits; use manual `Pin<Box<dyn Future>>` for traits requiring dyn dispatch (e.g. Tool)

## Codebase Map

```
 1  src
 2  ├── agents
 3  │   ├── context
 4  │   ├── director
 5  │   ├── executor
 6  │   └── implementer
 7  ├── cli
 8  │   └── dispatch
 9  ├── daemon
10  │   └── handlers
11  ├── decomposer
12  ├── domain
13  ├── evaluator
14  ├── ipc
15  ├── tests
16  │   ├── fsm
17  │   └── integration
18  ├── tools
19  │   └── builtin
20  ├── tui
21  │   ├── run
22  │   └── views
23  ├── validator
24  └── worktree
```

1. **src** - crate root
2. **agents** - all agent roles and shared infra (LLM client, session, worker pool, sandbox, events); includes `director.rs`/`implementer.rs`/`researcher.rs`/`reviewer.rs`/`decomposer.rs`/`generation.rs` at the top level alongside the subdirectories listed below
3. **context** - assembles parent_id chain + sibling context for LLM prompts
4. **director** - Director agent: long-lived event-driven loop with four modes (PlanIntake, Monitoring, Escalation, UserIntervention); structured action vocabulary in `actions.rs`
5. **executor** - drives one Work item through the agentic loop; owns lifecycle and LLM call
6. **implementer** - writes code inside a worktree
7. **cli** - clap command structure; subcommand dispatch via IPC; diagnose subcommand
8. **dispatch** - IPC dispatch logic and tests
9. **daemon** - background process: startup/shutdown, DaemonContext, agent supervisor, work queue
10. **handlers** - one file per IPC message type (includes `director.rs` for PlanIntake / user_message handlers)
11. **decomposer** - breaks high-level goals into Plan/Spec/Phase/Work hierarchy
12. **domain** - all persisted types (Plan/Spec/Phase/Work, Bundle, Chat, Lock, Tick, etc.); one file per type
13. **evaluator** - coverage evaluator: judges whether a Plan/Spec/Phase has adequate coverage
14. **ipc** - Unix socket IPC: protocol enums, param structs, client, server, framing codec
15. **tests** - test suites outside module-level unit tests
16. **fsm** - FSM transition correctness tests, one file per domain type
17. **integration** - full integration tests: spin up daemon, exercise pipeline end-to-end
18. **tools** - unified Tool trait, registry/router, agentic loop driver, sandbox, lane scheduling
19. **builtin** - one file per built-in tool (read, write, edit, glob, grep, shell, fetch, etc.)
20. **tui** - ratatui terminal UI: app state, keyboard input, event/render/IPC poll loop
21. **run** - event loop split into events, IPC polling, and render sub-modules
22. **views** - one file per TUI screen (dashboard, chat, agents, bundles, works, ticks, etc.)
23. **validator** - doc validator (sync/ureq): validates documents against templates in resources/decompose/
24. **worktree** - git worktree lifecycle: create, clean, merge back to main

## Key External Dependencies

- **TaskStore** (`scottidler/taskstore`) - JSONL-as-truth, SQLite-as-cache persistence. Generic over types implementing `Record` trait. Git dependency.
- **reqwest** - async HTTP client for agent LLM calls (streaming SSE). Used in Tokio task context.
- **ureq** - sync HTTP client for Doc Validator. Used in sync handler context.
- **glob** - glob pattern matching for Researcher SearchFiles action.

