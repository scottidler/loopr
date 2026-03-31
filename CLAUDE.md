# Loopr v3 - Claude Code Instructions

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

## Key External Dependencies

- **TaskStore** (`scottidler/taskstore`) - JSONL-as-truth, SQLite-as-cache persistence. Generic over types implementing `Record` trait. Git dependency.
- **reqwest** - async HTTP client for agent LLM calls (streaming SSE). Used in Tokio task context.
- **ureq** - sync HTTP client for Doc Validator. Used in sync handler context.
- **glob** - glob pattern matching for Researcher SearchFiles action.

