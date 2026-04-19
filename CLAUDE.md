# Loopr v5

Agent orchestrator with Plan decomposition as a central feature. Clean-break rewrite from v4; compiler-style pipeline of typed stages, each in its own crate.

## Start here

**[docs/vision.md](docs/vision.md)** is the front door. It defines the architectural shape, crate layout, ABI contracts, process rules, and first gate. Read it before changing anything non-trivial.

## Where to find things

- **Architectural shape (vision):** [docs/vision.md](docs/vision.md)
- **Roadmap (what's built, what's next):** [docs/roadmap.md](docs/roadmap.md)
- **Top-level docs rules:** [docs/CLAUDE.md](docs/CLAUDE.md)
- **Per-crate scope rules:** `crates/<name>/CLAUDE.md`
- **Per-crate design docs:** `crates/<name>/docs/`
- **Workspace CI:** `./.otto.yml` (run `otto ci` at repo root)
- **Per-crate CI:** `crates/<name>/.otto.yml` (run `otto ci` inside the crate dir)

## Crate map

| Crate | Role | Depends on |
|---|---|---|
| [derive](crates/derive/CLAUDE.md) | Proc macros (Fsm, Record); derives only, no fn-like or attribute macros | - |
| [telemetry](crates/telemetry/CLAUDE.md) | Tracing subscriber init, run-id, span conventions, log-query helpers | - |
| [store](crates/store/CLAUDE.md) | Typed wrapper around `scottidler/taskstore`; JSONL+SQLite+git-hooks anti-corruption layer | derive, taskstore |
| [domain](crates/domain/CLAUDE.md) | Records + FSM tables only (no I/O) | derive, taskstore |
| [llm](crates/llm/CLAUDE.md) | LlmClient trait + Anthropic backend (no prompt assembly — that's `agents`) | domain, telemetry |
| [tools](crates/tools/CLAUDE.md) | Tool trait + builtins + lane classification + bwrap sandbox | domain, telemetry |
| [worktree](crates/worktree/CLAUDE.md) | Sibling git worktrees + registry + crash recovery | domain, telemetry |
| [ipc](crates/ipc/CLAUDE.md) | Typed daemon-client wire protocol (messages + framing, no transport) | domain |
| [context](crates/context/CLAUDE.md) | Prompt assembly (handlebars + partials + token budgeting); shared by decomposer and agents | domain, store, tools, telemetry |
| [decomposer](crates/decomposer/CLAUDE.md) | Goal to Work DAG | domain, store, llm, context |
| [agents](crates/agents/CLAUDE.md) | Ralph loops per role | domain, store, llm, tools, worktree, context |
| [integrator](crates/integrator/CLAUDE.md) | Merge-validate-publish (non-LLM; no `llm` dep at Cargo level) | domain, store, worktree |
| [loopr](crates/loopr/CLAUDE.md) | Binary: daemon loop + CLI dispatch + IPC transport + (later) TUI launcher | all of the above |

## Working rules (v5-specific; user global rules still apply)

1. **One design doc at a time, motivated by a failing run.** No detailed spec without a failing E2E that motivates it. `docs/vision.md` is the exception, the seed.
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
