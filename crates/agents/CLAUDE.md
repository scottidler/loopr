# agents

Ralph Wiggum loops per role. The backend execute stages of the pipeline.

## In scope

- `run_implementer(work: &Work, ctx: &Context) -> Result<Bundle>`
- `run_reviewer(bundle: &Bundle, ctx: &Context) -> Result<Verdict>`
- `run_researcher(query: &Query, ctx: &Context) -> Result<Finding>`
- `run_director(event: &Event, ctx: &mut Context) -> Result<Action>`
- `RetryStrategy`, `EscalationStrategy` traits with named impls selected by config
- Role-specific prompts under `src/prompts/`
- Per-role `Config` sub-structs composed into this crate's `Config`

## Out of scope

- Decomposition (`decomposer`), integration (`integrator`)
- LLM transport, tool registry, context budget math (`runtime`)
- Record types and FSM transitions (`domain`); agents transition records but the rules are enforced in `domain`
- Which role to spawn when (that's the driver in `loopr`)

## Rule

A Ralph loop in this crate takes a typed input, runs against `runtime`, and returns a typed output. If a function here is making orchestration decisions ("also spawn a reviewer", "escalate to director"), pull that into the driver in `loopr` and emit an event instead.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/v5-shape.md](../../docs/v5-shape.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
