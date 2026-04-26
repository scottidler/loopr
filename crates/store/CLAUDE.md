# store

Type-safe wrapper around `scottidler/taskstore`. Owns the JSONL-is-truth + SQLite-as-cache persistence layer, per-target path resolution, and git-hooks install for the merge driver.

## In scope

- `Store` struct wrapping `taskstore::Store`; opens at `<target>/.loopr/taskstore/` on first call (`TASKSTORE_SUBPATH` in `store::store`)
- Type-safe collection accessors: `store.plans()`, `store.works()`, etc., returning handles that preserve record types instead of raw `taskstore` generics
- Path resolution relative to the effective target (`loopr -C <path>` or CWD)
- `install-hooks`: invokes `taskstore install-hooks` as part of `loopr init`
- Store-level error types; all `rusqlite` / `fs2` leakage stops here
- Config: `StoreConfig` composed into the top-level `Config` by `loopr`

## Out of scope

- Record type definitions (`Plan`, `Spec`, `Phase`, `Work`, `Bundle`, `Tick`) and their FSM tables — those live in `domain`
- The `Record` trait itself — that lives in `taskstore-traits` (and is re-exported by `taskstore`)
- Orchestration decisions about what to store when — that's each stage crate's business
- Any LLM, tool, or worktree concern

## Rule

This crate is the anti-corruption layer between loopr's domain types and taskstore's persistence engine. `rusqlite`, `fs2`, and any `taskstore`-internal types should NOT leak out of this crate's public API. Downstream crates see typed `StoreError`, typed collection accessors, and nothing else.

The Round 1 Architect critique — "domain should not depend on an I/O-bound persistence engine" — is precisely this crate's reason to exist. `domain` keeps records + FSM pure; `store` holds everything else.

## Update sinks

The crate exposes three `*UpdateSink` traits — `BundleUpdateSink`, `WorkUpdateSink`, `PlanUpdateSink` — that mirror the per-collection `update` methods at a per-record granularity. Real impls forward to the corresponding `Store::*` methods; `&S` and `Arc<S>` forwarding impls let callers pass `&self.store` or `Arc::clone(&self.store)` without unwrapping. The daemon's `SummaryFanout` decorator implements all three so per-record `summary.md` files land transactionally with each FSM transition. `PlanUpdateSink::update(plan, children)` carries siblings explicitly (option (c-extended) in the design): the caller fetches children before invoking, the renderer sees both arguments. Bundles use OCC and surface `BundleUpdateError::Stale` separately so the Reviewer/Integrator retry path can match on it.

## Corruption-tolerant reads

`BundlesStore::list_tolerant(&[Filter])` and `WorksStore::list_tolerant(&[Filter])` forward to `taskstore_async::AsyncStore::list_tolerant`, which reads the JSONL files directly (bypassing the SQLite cache) and returns `ListResult<T> { records, corruption }`. Per-row failures surface as `CorruptionEntry` instead of either failing the whole list or silently dropping at `sync()`. `StoreError::Corruption` is reserved for future failure modes that don't have a per-row fallback; the current daemon uses `list_tolerant`'s data-as-corruption shape exclusively. Recovery path: `git -C <target> checkout HEAD -- .loopr/taskstore/` (the JSONL files are git-tracked; SQLite is just a cache).

## Dependencies

`taskstore` (git dep, inherited via `workspace = true` from the root `[workspace.dependencies]` block), `derive`, and workspace-shared crates (`eyre`, `tracing`, `serde`). Added via `cargo add` at the time the first code needs them, not speculatively.

### Note on the `taskstore-traits` split

`scottidler/taskstore` is a two-crate workspace: `taskstore-traits` (trait-only, pure `serde` + `std`) and `taskstore` (Store engine + `rusqlite`/`fs2`/`chrono`). `domain` depends on traits-only; `store` depends on the full crate because it needs `Store`. Both deps MUST resolve to the same commit — they're declared centrally in the root `Cargo.toml` for exactly that reason. See `../../docs/taskstore-integration.md` for the split-brain failure mode that motivates the centralized declaration.

## Instrumentation

Every public method on `PlansStore`, `WorksStore`, `BundlesStore`, `TicksStore`, plus `Store::open` and `Store::close`, opens a span at `debug` (open/close at `info`) with `err`. Required scope fields:

- `record_kind` — `plan` / `work` / `bundle` / `tick`. Constant per collection.
- `record_id` — present on `create`, `get`, `update` (and `record_kind`-prefixed lists carry the parent id, e.g. `parent_id` on `works.list_by_parent_id`).
- `op` — `create` / `create_many` / `get` / `list` / `list_by_*` / `update`. Stable string per method.
- `count` — recorded on every `list*` after the query returns; lets readers compare expected vs actual cardinality.
- Domain-specific: `parent_id` on Work create, `work_id` on Bundle create/update, `force_proposed` on Bundle create, `expected_updated_at` on Bundle update (the OCC version), `bundle_count` on Tick create.

Writes (`create`, `create_many`, `update`) carry `ret` so the span close logs the returned id. Reads carry `err` only.

The acceptance test `tests/instrumentation.rs` opens a tempdir Store, exercises every method on every collection, and asserts each span exists with its required keys.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape, "Target Repo Layout" section
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
