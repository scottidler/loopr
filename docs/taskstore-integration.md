# Integrating taskstore and taskstore-traits

`scottidler/taskstore` is now a two-crate workspace:

| Crate | What it gives you | Deps |
|-------|-------------------|------|
| `taskstore-traits` | `Record` trait, `IndexValue`, `Filter`, `FilterOp` | `serde` only |
| `taskstore` | Full `Store` engine, JSONL/SQLite persistence, CLI | `rusqlite`, `fs2`, `chrono`, etc. |

The split exists so loopr-v5's `domain` crate (pure symbol layer, no I/O) can depend on
just the trait surface without pulling in the storage engine.

## How to wire it up

Declare both deps centrally in the **root** `Cargo.toml` and inherit with `workspace = true`
in member crates. This is load-bearing: it structurally prevents the split-brain failure mode
described below.

```toml
# loopr-v5/Cargo.toml  (root workspace manifest)
[workspace.dependencies]
taskstore-traits = { git = "ssh://git@github.com/scottidler/taskstore", branch = "main" }
taskstore        = { git = "ssh://git@github.com/scottidler/taskstore", branch = "main" }
```

```toml
# crates/domain/Cargo.toml  (pure symbol layer - no I/O)
[dependencies]
taskstore-traits = { workspace = true }
```

```toml
# crates/store/Cargo.toml  (persistence layer - needs Store)
[dependencies]
taskstore = { workspace = true }
```

## Import paths

Both forms compile - use whichever reads naturally:

```rust
// Flat imports (recommended for most use)
use taskstore_traits::{Record, IndexValue, Filter, FilterOp};

// From the full crate (if you already depend on taskstore)
use taskstore::{Record, IndexValue, Filter, FilterOp};       // flat
use taskstore::record::{Record, IndexValue};                  // module path
use taskstore::filter::{Filter, FilterOp};                    // module path
```

## The split-brain failure mode (why workspace = true matters)

If two members of this workspace ever declare `taskstore-traits` with different git source
pointers (different `rev`, different tags pointing to different commits), Cargo instantiates
`taskstore-traits` twice. Because Rust's type identity is tied to the crate instance,
`taskstore_traits::Record` from commit X and `taskstore_traits::Record` from commit Y are
incompatible - even if the source is byte-identical. You get errors like:

```
expected trait `taskstore_traits::Record`, found trait `taskstore_traits::Record`
```

This wastes hours before the cause clicks. The `[workspace.dependencies]` pattern makes it
structurally impossible: there is one declaration per crate, so all members resolve to the
same commit.

## Pinning options

| Style | How | Safe? |
|-------|-----|-------|
| Track main | `branch = "main"` on both (recommended default) | Yes |
| Pin to release | `tag = "v0.3.0"` on both | Yes |
| Reproducible | `rev = "<sha>"` on both, same sha | Yes |

## Versioning

Both crates are tagged together with a single flat `v*` tag on the workspace, same as before the split. `v0.3.0` is the first workspace release. Pin by tag or branch - no per-crate prefixes.
