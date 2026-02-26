# Loopr v3 — Claude Code Instructions

## Project

Loopr is a TUI-based "dev team in a box" orchestrator. MVP1 (orchestration spine) is complete. We're building MVP2 — TaskStore persistence and read-only Doc Validator LLM.

## Source of Truth

The design doc at `docs/design/2026-02-26-loopr-v3-mvp2.md` is the single source of truth for MVP2. The MVP1 design doc at `docs/design/2026-02-25-loopr-v3-mvp1.md` remains the reference for the existing spine architecture. Do not deviate from them without discussion.

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

## Key External Dependency

TaskStore (`scottidler/taskstore`) — JSONL-as-truth, SQLite-as-cache persistence. Generic over types implementing `Record` trait. This is a git dependency.

## Ralph Wiggum Loop

When running in the loop (`bin/loop.sh`), Claude reads `PROMPT.md` each iteration with fresh context. State persists in `progress.txt` and git history only. The loop runs `otto ci` externally after each iteration.
