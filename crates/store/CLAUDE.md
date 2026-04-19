# store

Type-safe wrapper around `scottidler/taskstore`. Owns the JSONL-is-truth + SQLite-as-cache persistence layer, per-target path resolution, and git-hooks install for the merge driver.

## In scope

- `Store` struct wrapping `taskstore::Store`; opens at `<target>/.taskstore/` on first call
- Type-safe collection accessors: `store.plans()`, `store.works()`, etc., returning handles that preserve record types instead of raw `taskstore` generics
- Path resolution relative to the effective target (`loopr -C <path>` or CWD)
- `install-hooks`: invokes `taskstore install-hooks` as part of `loopr init`
- Store-level error types; all `rusqlite` / `fs2` leakage stops here
- Config: `StoreConfig` composed into the top-level `Config` by `loopr`

## Out of scope

- Record type definitions (`Plan`, `Spec`, `Phase`, `Work`, `Bundle`, `Tick`) and their FSM tables — those live in `domain`
- The `Record` trait itself — that lives in `taskstore` (or `taskstore-traits` after the upstream split)
- Orchestration decisions about what to store when — that's each stage crate's business
- Any LLM, tool, or worktree concern

## Rule

This crate is the anti-corruption layer between loopr's domain types and taskstore's persistence engine. `rusqlite`, `fs2`, and any `taskstore`-internal types should NOT leak out of this crate's public API. Downstream crates see typed `StoreError`, typed collection accessors, and nothing else.

The Round 1 Architect critique — "domain should not depend on an I/O-bound persistence engine" — is precisely this crate's reason to exist. `domain` keeps records + FSM pure; `store` holds everything else.

## Dependencies

`taskstore` (git dep), `derive`, and workspace-shared crates (`eyre`, `tracing`, `serde`). Added via `cargo add` at the time the first code needs them, not speculatively.

### Note on `taskstore-traits` split

`domain` only needs `taskstore::Record` (the trait). Depending on the full `taskstore` crate transitively pulls in `rusqlite` (bundled), `fs2`, `tracing-subscriber`, `chrono`. An upstream PR to `scottidler/taskstore` should extract `taskstore-traits` (just `Record`, `IndexValue`, `Filter`) from `taskstore/src/record.rs` — trait content uses only `serde` + `std`. After that split, `domain` depends on `taskstore-traits`; `store` depends on full `taskstore`. See `docs/vision.md` note.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape, "Target Repo Layout" section
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
