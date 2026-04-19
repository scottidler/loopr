# integrator

Deterministic, non-LLM merge-validate-publish. Accepted Bundles into Tick.

## In scope

- `fn integrate(bundles: &[Bundle]) -> Result<Tick, IntegrationError>`
- Sequential git merge of bundle branches into the integration branch
- Conflict detection and classification (structural vs retryable)
- Validation command execution (via `runtime`) and result capture
- Tick record creation and SHA capture
- This crate's own `Config` struct

## Out of scope

- LLM calls of any kind; this crate is the non-LLM chokepoint of the pipeline
- Agent spawning or decisions about retry (`agents`, `loopr`)
- Decomposition (`decomposer`), domain records (`domain`)
- What to do when integration fails; this crate returns a typed error and the driver decides

## Rule

This crate must not depend on `LlmClient`. Given the same bundles and the same base commit, `integrate` produces the same Tick SHA or the same typed error. That determinism is the invariant; keep it.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
