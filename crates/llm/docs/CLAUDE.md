# crates/llm/docs/

Design documentation scoped to the `llm` crate.

## What lives here

- Design docs touching only `llm` (API-bounds trait, Anthropic backend, SSE streaming, model tier resolution, retry on transient errors).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Prompt assembly / `ContextBuilder` — that lives in `agents`, not `llm`.
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `llm`
- [../../../docs/vision.md](../../../docs/vision.md): "Models and Budgets" section
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules
