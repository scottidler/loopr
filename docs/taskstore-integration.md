# Integrating taskstore, taskstore-traits, and taskstore-async

`scottidler/taskstore` is a three-crate workspace (as of `v0.5.0`):

| Crate | What it gives you | Deps |
|-------|-------------------|------|
| `taskstore-traits` | `Record` trait, `IndexValue`, `Filter`, `FilterOp` | `serde` only |
| `taskstore`        | Full `Store` engine, JSONL/SQLite persistence, CLI, `taskstore-merge` driver | `rusqlite`, `fs2`, `chrono`, etc. |
| `taskstore-async`  | Async-native `AsyncStore` for tokio consumers | `tokio`, re-exports from `taskstore` |

The split exists so loopr-v5's `domain` crate (pure symbol layer, no I/O) can depend on
just the trait surface without pulling in the storage engine; `store` uses `taskstore-async`
directly for daemon-side persistence, and `taskstore-merge` stays sync because `git merge`
invokes it with no tokio runtime.

## How to wire it up

Declare both deps centrally in the **root** `Cargo.toml` and inherit with `workspace = true`
in member crates. This is load-bearing: it structurally prevents the split-brain failure mode
described below.

```toml
# loopr-v5/Cargo.toml  (root workspace manifest)
[workspace.dependencies]
taskstore-traits = { git = "ssh://git@github.com/scottidler/taskstore", tag = "v0.5.0" }
taskstore        = { git = "ssh://git@github.com/scottidler/taskstore", tag = "v0.5.0" }
taskstore-async  = { git = "ssh://git@github.com/scottidler/taskstore", tag = "v0.5.0" }
```

```toml
# crates/domain/Cargo.toml  (pure symbol layer - no I/O)
[dependencies]
taskstore-traits = { workspace = true }
```

```toml
# crates/store/Cargo.toml  (async persistence layer - needs AsyncStore)
[dependencies]
taskstore-async = { workspace = true }
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

## Pinning

Pin all three entries to the **same flat `v*` tag** on the taskstore workspace. Branch-
tracking (`branch = "main"`) is deliberately rejected for loopr-v5: `cargo install`
re-resolves the lockfile and will silently pull a newer commit, which is how the v0.5.13
install broke (taskstore's `AsyncStore::open` signature changed on main between the
workspace lock and the install-time resolution).

| Style | How | Use |
|-------|-----|-----|
| Pin to release (current) | `tag = "v0.5.0"` on all three | Default |
| Reproducible to a commit | `rev = "<sha>"` on all three, same sha | When no release tag yet covers the fix |
| Track main | `branch = "main"` on all three | **Do not use** — `cargo install` can skew to newer commits |

## Versioning

All three crates ship under a single flat `v*` tag on the taskstore workspace. Per-crate
semver (`taskstore 0.5.0`, `taskstore-async 0.2.0`, `taskstore-traits 0.1.0`) rides on
top of that one tag; the workspace tag, not the per-crate version, is what loopr-v5 pins
to. No per-crate-prefixed tags.
