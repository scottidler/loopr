# domain

Records + FSM transition tables. The pure symbol layer of v5. No I/O, no persistence, no network.

## In scope

- Record types: `Plan`, `Spec`, `Phase`, `Work`, `Bundle`, `Tick`, and their FSMs
- Const transition tables with role guards via `#[derive(Fsm)]` from `crates/derive/`
- `Record` trait impls (the trait itself lives in `taskstore-traits`)
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

Source code in this crate must not `use` anything from `tokio`, `reqwest`, `ureq`, `rusqlite`, or any I/O-bound dependency. The Round 1 Architect critique — "a pure symbol layer should not host SQLite caches and JSONL files" — is what drove extracting `Store` into its own crate; don't re-introduce the coupling.

## Dependency on `taskstore-traits`

`domain` depends on `taskstore-traits` only (not the full `taskstore` crate). The traits crate holds `Record`, `IndexValue`, `Filter`, `FilterOp` — pure `serde` + `std`, no I/O. This is a foundational dep, same category as `serde::Serialize`: it's what makes domain records persistable by downstream layers.

The dep is declared in the root `[workspace.dependencies]` block as a git source pinned to a flat tag (`tag = "v0.5.0"`; all three taskstore crates share the same workspace tag) and inherited here via `workspace = true`. Never declare a second independent source for `taskstore-traits` in this crate — the root declaration is load-bearing for the same-commit guarantee (see `../../docs/taskstore-integration.md` for the split-brain failure mode and why branch-tracking is rejected here).

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape, "Target Repo Layout" for how `domain` types land on disk via `store`
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
