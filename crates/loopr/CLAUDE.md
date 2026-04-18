# loopr

The driver. Binary crate: daemon process, IPC, TUI, CLI dispatch.

## In scope

- `main.rs`: thin shell that parses args and forks-to-daemon or connects-as-client
- `lib.rs`: daemon, ipc, tui, cli modules
- Daemon: owns `TaskStore`, runs the reactive loop, threads records through stage crates, emits events
- IPC: Unix socket, NDJSON framing, versioned protocol (from `docs/v2-proven-patterns.md`)
- TUI: ratatui views, acts as the visual debugger for the pipeline
- CLI: subcommands mirror stage boundaries (`loopr plan`, `loopr decompose`, `loopr execute`, `loopr integrate`, `loopr experiment`)
- Top-level `Config` that composes each stage crate's `Config`
- Experiment harness: runs a config against a target repo, emits a score

## Out of scope

- Any stage logic; that lives in the corresponding stage crate
- LLM client, tool impls, worktree lifecycle (`runtime`)
- Record types (`domain`)
- Decomposition math (`decomposer`), agent loops (`agents`), integration logic (`integrator`)

## Rule

This crate orchestrates. It does not implement any pipeline stage. If you catch yourself writing decomposition logic or an agent loop here, move it to the stage crate and call it from the driver.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/v5-shape.md](../../docs/v5-shape.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
