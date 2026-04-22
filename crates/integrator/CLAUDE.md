# integrator

Deterministic, non-LLM merge-validate-publish. Accepted Bundles into Tick.

## In scope

- `fn integrate(bundles: &[Bundle]) -> Result<Tick, IntegrationError>`
- Sequential git merge of bundle branches into the integration branch (`loopr/plan-<plan-id>`)
- Conflict detection and classification (structural vs retryable)
- Validation command execution (via `tools`, the `Heavy` lane specifically) and result capture
- Tick record creation and SHA capture, persisted via `store`
- This crate's own `Config` struct

## Out of scope

- **LLM calls of any kind.** This crate is the non-LLM chokepoint of the pipeline. Dependencies: `domain`, `store`, `worktree` — emphatically NOT `llm`. The Round 1 Architect flagged this as a contradiction in the previous `runtime`-monolith design; the crate restructure makes the rule mechanically enforceable at the Cargo graph level.
- Tools trait and impls (`tools`). Integrator can *invoke* a tool via a passed `ToolExecutor` handle (dependency injection), but does not own the registry.
- Agent spawning or decisions about retry (`agents`, `loopr`)
- Decomposition (`decomposer`), domain records (`domain`)
- What to do when integration fails; this crate returns a typed error and the driver decides

## Rule

This crate must not depend on `llm`. Compile-time enforced via Cargo: `integrator`'s `[dependencies]` never includes `llm`. If you find yourself reaching for an LLM here, the logic belongs in `agents`; emit an event for the driver instead.

Given the same bundles and the same base commit, `integrate` produces the same Tick SHA or the same typed error. That determinism is the invariant; keep it.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
