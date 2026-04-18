# Loopr v5

Agent orchestrator with Plan decomposition as a central feature. Clean-break rewrite from v4; compiler-style pipeline of typed stages, each in its own crate.

## Where to find things

- **Architectural shape (vision):** [docs/v5-shape.md](docs/v5-shape.md)
- **Top-level docs rules:** [docs/CLAUDE.md](docs/CLAUDE.md)
- **Per-crate scope rules:** `crates/<name>/CLAUDE.md`
- **Per-crate design docs:** `crates/<name>/docs/`
- **Workspace CI:** `./.otto.yml` (run `otto ci` at repo root)
- **Per-crate CI:** `crates/<name>/.otto.yml` (run `otto ci` inside the crate dir)

## Crate map

| Crate | Role | Depends on |
|---|---|---|
| [domain](crates/domain/CLAUDE.md) | Records, FSM tables, TaskStore wrapper | - |
| [runtime](crates/runtime/CLAUDE.md) | LLM, tools, context, worktrees | domain |
| [decomposer](crates/decomposer/CLAUDE.md) | Goal to Work DAG | domain, runtime |
| [agents](crates/agents/CLAUDE.md) | Ralph loops per role | domain, runtime |
| [integrator](crates/integrator/CLAUDE.md) | Merge-validate-publish (non-LLM) | domain, runtime |
| [loopr](crates/loopr/CLAUDE.md) | Binary: daemon + IPC + TUI + CLI | all of the above |

## Working rules (v5-specific; user global rules still apply)

1. **One design doc at a time, motivated by a failing run.** No detailed spec without a failing E2E that motivates it. `docs/v5-shape.md` is the exception, the seed.
2. **Seam tests, not only unit tests.** Every crate boundary has at least one round-trip serde test and one integration test that crosses the seam with real types.
3. **No coexistence migrations.** A paradigm change replaces its predecessor in one commit, not dual-pathed. v3 to v4 coexistence failed; v5 does not repeat that.
4. **The crate is the unit of blast radius.** A mistake in one crate must be recoverable without touching the others. A PR that touches two or more crates is a deliberate cross-cutting change and needs a top-level design doc.

## Build

```
otto ci                       at repo root: exercises the whole workspace
otto ci                       inside any crate dir: exercises just that crate
cargo check --workspace       quick compile check
cargo test -p <crate>         test one crate
```

## Versioning

Single `workspace.package.version` in root `Cargo.toml`, inherited by every crate via `version.workspace = true`. Repo tag equals workspace version equals every crate's version. Use `bump` to advance.
