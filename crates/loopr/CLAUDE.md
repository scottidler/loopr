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

## Summary generators

Per-record markdown digests live under `<target>/.loopr/records/<kind>/<id>/summary.md`. The renderers in `crates/loopr/src/summary/` are pure (input record + extra context = `String`); the writers atomic (write-to-temp + rename). Renderers covered: `render_bundle`, `render_work`, `render_plan(plan, &[Work])`. Each has a unit test asserting required sections + post-write file existence.

Best-effort `write_<kind>_summary_best_effort` helpers in `crates/loopr/src/daemon/context.rs` log a `warn!` on failure and return; transcripts and FSM transitions never propagate a summary error.

**Wiring status:** the renderer + atomic-write surface ships in this phase; per-transition callsite wiring at `spawn_implementer_for_work`, `spawn_reviewer_for_bundle`, and `spawn_integrator_for_bundle` remains a focused follow-up. The design doc Phase 8.5 assumed `WorkUpdateSink` / `PlanUpdateSink` traits that mirror `BundleUpdateSink`; only the latter exists, so per-transition fanout is best added once the daemon is being touched anyway. No regression: a missing summary is regenerated on the next transition that does touch the right callsite, and the renderers are deterministic given taskstore state.

**Process / session digests** (`runs/<process-id>/summary.md` and `sessions/<session-id>/summary.md`) are not yet built; the design doc itself notes these depend on the daemon shutdown hook and `loopr sessions end`. Tracked as a follow-up alongside the wiring.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
