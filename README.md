# Loopr v5

Agent orchestrator with Plan decomposition as a central feature. Clean-break rewrite from v4 on an orphan branch; compiler-style pipeline of typed stages, each in its own crate.

## Start here

- **[docs/vision.md](docs/vision.md)** - architectural shape, crate layout, process rules, first gate. Read this first.
- **[docs/roadmap.md](docs/roadmap.md)** - living index of stages and design docs. Where work goes next.
- **[CLAUDE.md](CLAUDE.md)** - project-wide rules and the canonical crate map.
- **[crates/](crates/)** - each crate has its own `CLAUDE.md` (scope rules) and `docs/` (design docs).

## Build

```
cargo check --workspace        quick compile check
otto ci                        full lint + check + test at the workspace root
otto ci                        inside any crate dir: scoped to that crate
cargo test -p <crate>          test one crate
```

## Versioning

Single `workspace.package.version` in root `Cargo.toml`, inherited by every crate via `version.workspace = true`. Repo annotated tag equals workspace version equals every crate's version. Use `bump` to advance.
