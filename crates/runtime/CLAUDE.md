# runtime

Effectful services agents and stages consume: LLM, tools, context, worktrees.

## In scope

- `LlmClient` trait and the default Anthropic Messages impl with SSE streaming
- `Tool` trait with typed `Input`/`Output`, one builtin per file under `src/tools/`
- `ContextBuilder`: token-budgeted prompt assembly
- `Worktree` handle with guaranteed cleanup (`Drop` or explicit finalize)
- Sandbox wrapping for shell execution (`bwrap` on Linux)
- This crate's own `Config` struct

## Out of scope

- Record types (`domain`), FSM transitions (`domain`), TaskStore semantics (`domain`)
- Any orchestration decision (what to do next, which stage to run, which role to spawn)
- Plan semantics; this crate does not know what a `Plan` means, only that a record with a `Record` impl can be fetched and mutated
- Stage-specific prompts (those live alongside each stage's agent logic)

## Rule

Stages and agents depend on `runtime` as a generic services layer. `runtime` itself depends only on `domain`. If you find yourself matching on `Work.status` or deciding "when a Bundle is Accepted, do X" here, you're in the wrong crate.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/v5-shape.md](../../docs/v5-shape.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
