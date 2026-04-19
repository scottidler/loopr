# domain

Records + FSM transition tables. The pure symbol layer of v5. No I/O, no persistence, no network.

## In scope

- Record types: `Plan`, `Spec`, `Phase`, `Work`, `Bundle`, `Tick`, and their FSMs
- Const transition tables with role guards via `#[derive(Fsm)]` from `crates/derive/`
- `Record` trait impls (the trait itself lives in `taskstore` / `taskstore-traits` — see dep note below)
- Shared enums: `Status`, `Role`, `Tier`
- Typed IDs: `PlanId`, `WorkId`, `RunId`, `BundleId`, etc.
- Serde types with `deny_unknown_fields`
- This crate's own `Config` struct (composed into the top-level `Config` by `loopr`)

## Out of scope

- **Persistence / `Store`.** That lives in `store`. `domain` defines the types; `store` handles JSONL + SQLite + git hooks.
- LLM calls — those live in `llm`
- Tool execution — that's `tools`
- Git operations, worktree lifecycle — that's `worktree`
- Plan decomposition (`decomposer`), agent execution (`agents`), integration (`integrator`)
- Any orchestration decision (that's `loopr`)

## Rule

This crate must compile without `tokio`, `reqwest`, `ureq`, `rusqlite`, or any I/O-bound dependency. If you reach for one here, the code belongs elsewhere. The Round 1 Architect critique — "a pure symbol layer should not host SQLite caches and JSONL files" — is what drove extracting `Store` into its own crate; don't re-introduce the coupling.

## Dependency on `taskstore`

`domain` uses `taskstore::Record` (the trait) to make records persistable. This is a foundational dep, same category as `serde::Serialize`. The trait itself lives in `taskstore/src/record.rs` and uses only `serde` + `std`.

**Known pending work:** the current `taskstore` crate (v0.2.3) bundles the `Record` trait alongside `Store` (which pulls in `rusqlite`, `fs2`, `tracing-subscriber`, `chrono`). Depending on the full `taskstore` transitively imports all of that into `domain`, which weakens the "pure symbol layer" claim. An upstream PR to `scottidler/taskstore` should extract `taskstore-traits` (just `Record`, `IndexValue`, `Filter`) so `domain` can depend on the traits crate only. Tracked in `docs/vision.md` as a pending upstream task; not a v5 blocker but scheduled before Stage 5 (when Records start being written).

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape, "Target Repo Layout" for how `domain` types land on disk via `store`
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
