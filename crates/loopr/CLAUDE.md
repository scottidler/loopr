# loopr

The driver. Binary crate: daemon process, IPC transport, CLI dispatch, source-guard and `-C` flag handling.

## In scope

- `main.rs`: thin shell that parses args, checks the source-guard, resolves the effective target (`-C <path>` or CWD), and forks-to-daemon or connects-as-client
- `lib.rs`: daemon, transport, cli modules
- Daemon: owns `TaskStore`, runs the reactive loop, threads records through stage crates, emits events via the `telemetry` crate
- **IPC transport:** Unix socket bind/accept, `tokio::net::UnixListener`/`UnixStream`, `LinesCodec` hookup with the 1 MiB max line from v3/v4, client connect lifecycle, stale-socket detection, PID lockfile. Protocol definitions (message types, framing choice, error codes) live in `crates/ipc` and are imported here.
- Fork-to-daemon mechanics and daemon-client coordination (pattern from `docs/v2-proven-patterns.md` in the v4 tree)
- **Session lifecycle.** `session.rs` resolves the process's `SessionId` at startup (from `--session`, the `.loopr/active-session` pointer, or a fresh allocation), and owns the `loopr sessions {list, new, resume, end, status}` verb bodies that manipulate that pointer plus the XDG-backed session manifests.
- CLI: subcommands mirror stage boundaries (`loopr plan`, `loopr decompose`, `loopr execute`, `loopr integrate`, `loopr experiment`, `loopr logs`, `loopr sessions`)
- `loopr init`: idempotent per-target setup (create `.loopr/`, open TaskStore, install taskstore git hooks, append to `.git/info/exclude`, verify source-guard)
- Top-level `Config` that composes each stage crate's `Config`
- Experiment harness: runs a config against a target repo, emits a score

## Out of scope

- Any stage logic; that lives in the corresponding stage crate
- **Protocol definitions** (`DaemonRequest`/`DaemonResponse`/`DaemonEvent`, NDJSON framing choice, RPC error codes) — those live in `crates/ipc`. If you find yourself defining a new message variant here, move it to `ipc` and import it
- LLM client (`llm`), tool impls (`tools`), worktree lifecycle (`worktree`), persistence (`store`)
- Record types (`domain`)
- Decomposition math (`decomposer`), agent loops (`agents`), integration logic (`integrator`)
- Tracing subscriber init, span conventions, session-id / process-id / target-slug generation, XDG path composition (`telemetry`)
- **TUI:** deferred to its own future crate. When it lands, `loopr` may spawn or exec into it; rendering never lives here

## Rule

This crate orchestrates. It does not implement any pipeline stage. If you catch yourself writing decomposition logic or an agent loop here, move it to the stage crate and call it from the driver.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
