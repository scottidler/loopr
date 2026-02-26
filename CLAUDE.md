# Loopr v3 — Claude Code Instructions

## Project

Loopr is a TUI-based "dev team in a box" orchestrator. MVP1 (orchestration spine) and MVP2 (TaskStore persistence + Doc Validator) are complete. We're building MVP3 — LLM agents, tool execution, and streaming.

## Source of Truth

The design doc at `docs/design/2026-02-26-loopr-v3-mvp3.md` is the single source of truth for MVP3. Previous MVP design docs remain references for the existing architecture:
- `docs/design/2026-02-25-loopr-v3-mvp1.md` — orchestration spine
- `docs/design/2026-02-26-loopr-v3-mvp2.md` — TaskStore + Doc Validator

Do not deviate from them without discussion.

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
- `cargo add` for dependencies — never hand-write versions
- Tests in every module (`#[cfg(test)] mod tests { ... }`)
- No `#[allow(dead_code)]` in final code
- No underscore-prefixed unused variables in final code
- Commit messages: `feat(scope): description` with phase context

## Key External Dependencies

- **TaskStore** (`scottidler/taskstore`) — JSONL-as-truth, SQLite-as-cache persistence. Generic over types implementing `Record` trait. Git dependency.
- **reqwest** — async HTTP client for agent LLM calls (streaming SSE). Used in Tokio task context.
- **ureq** — sync HTTP client for Doc Validator (MVP2, unchanged). Used in sync handler context.

## Ralph Wiggum Loop

When running in the loop (`bin/loop.sh`), Claude reads `PROMPT.md` each iteration with fresh context. State persists in `progress.txt` and git history only. The loop runs `otto ci` externally after each iteration.
