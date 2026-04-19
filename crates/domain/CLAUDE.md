# domain

Domain records, FSM transition tables, TaskStore wrapper. Pure data and invariants.

## In scope

- Record types: `Plan`, `Spec`, `Phase`, `Work`, `Bundle`, `Tick`, and their FSMs
- Const transition tables with role guards; `#[derive(Fsm)]` or hand-rolled const tables
- `Record` impls for TaskStore persistence
- Shared enums: `Status`, `Role`, `Tier`
- Serde types with `deny_unknown_fields`
- This crate's own `Config` struct (composed into the top-level `Config` by `loopr`)

## Out of scope

- LLM calls; those live in `runtime`
- Tool execution, shell, network, filesystem beyond TaskStore; those live in `runtime`
- Git operations, worktree lifecycle; those live in `runtime`
- Plan decomposition (that's `decomposer`), agent execution (`agents`), integration (`integrator`)
- Any orchestration decision (that's `loopr`)

## Rule

This crate must compile without `tokio`, `reqwest`, `ureq`, or any network/LLM dependency. If you reach for one here, the code belongs in `runtime`.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
