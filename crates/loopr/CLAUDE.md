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

## Instrumentation (daemon side)

The daemon's IPC dispatch and pipeline-spawn methods carry `#[tracing::instrument]`:

- `ipc.dispatch` (info) — every request, fields(`request_id`, `method`, `handshake_state`).
- `ipc.handshake` is recorded inline; `ipc.plan_create` (info, `goal_len`, post-record `plan_id`), `ipc.record_list` (debug, `kind`), `ipc.record_get` (debug, `record_id`), `ipc.status` (debug).
- `daemon.run_active`, `daemon.build_context`, `daemon.serve_core` — info+err with `target`/`session_id`/`process_id`/`target_slug`/`pid` so a daemon-startup failure carries every identifier needed to correlate against XDG paths.
- `daemon.reconcile`, `daemon.sweep_worktrees` — info/debug+err.
- `daemon.spawn_implementer_for_work`, `daemon.spawn_reviewer_for_bundle`, `daemon.spawn_integrator_for_bundle` — info, work_id/bundle_id + status fields. Every event emitted while these run inherits the relevant ids automatically.

Per-request scope fields available on the daemon's `ipc.connection` span (set in transport/server.rs at handshake completion): `client_session_id` and `request_id` propagate from the connection span downward into every handler's span.

## Transcripts

`crates/loopr/src/transcript/` holds the LLM round-trip transcript writers. Layout:

- `model.rs` — `TranscriptIteration` struct (model, started_at, latency_ms, prompt/completion tokens, session/process ids, events.log path, system prompt, user prompt, response, parsed actions, dispatcher outcomes, lifeguard decision).
- `render.rs` — `render_iteration(&TranscriptIteration) -> String` produces the markdown block; `redact_paths(text, &[String]) -> String` replaces lines containing any deny-pattern substring with `[redacted: pattern=<p>]`. Per-section cap: `ITERATION_BYTE_CAP / 4` (25 KB), enforced at render time. Truncation marker is the literal `>[truncated: N KB original; sha=<8>]<` from the design doc Q5.
- `mod.rs` — `append_iteration(path, &iter)` opens the file with `create+append`, writes the rendered block, and emits a `tracing::debug!("transcript_appended", path, iteration, bytes)` event after each append.

Paths under `<target>/.loopr/records/`:

- Decomposer: `plans/<plan-id>/decomposition.md`
- Implementer: `works/<work-id>/transcript.md` (append-only across iterations)
- Reviewer: `bundles/<bundle-id>/review.md`

`.git/info/exclude` is updated by `worktree::ensure_loopr_excludes` to cover `.loopr/records/`. Transcripts never get committed.

**Wiring status:** the model + renderer + atomic-append surface ships in this phase; agent-side population (decomposer / implementer / reviewer) remains a follow-up tracked alongside the summary-callsite wiring. The contract is: agents construct a `TranscriptIteration` with everything they have at the end of an LLM call, then call `append_iteration(transcript_path, &iter)`. Failures emit a `warn!` and the agent continues.

**System-prompt elision** (the design doc's "leaning yes" open question) is not implemented yet; iterations 2..N currently re-render the full system prompt. Tracked as a follow-up.

## Summary generators

Per-record markdown digests live under `<target>/.loopr/records/<kind>/<id>/summary.md`. The renderers in `crates/loopr/src/summary/` are pure (input record + extra context = `String`); the writers atomic (write-to-temp + rename). Renderers covered: `render_bundle`, `render_work`, `render_plan(plan, &[Work])`. Each has a unit test asserting required sections + post-write file existence.

Best-effort `write_<kind>_summary_best_effort` helpers in `crates/loopr/src/daemon/context.rs` log a `warn!` on failure and return; transcripts and FSM transitions never propagate a summary error.

**Wiring status:** the renderer + atomic-write surface ships in this phase; per-transition callsite wiring at `spawn_implementer_for_work`, `spawn_reviewer_for_bundle`, and `spawn_integrator_for_bundle` remains a focused follow-up. The design doc Phase 8.5 assumed `WorkUpdateSink` / `PlanUpdateSink` traits that mirror `BundleUpdateSink`; only the latter exists, so per-transition fanout is best added once the daemon is being touched anyway. No regression: a missing summary is regenerated on the next transition that does touch the right callsite, and the renderers are deterministic given taskstore state.

**Process / session digests** (`runs/<process-id>/summary.md` and `sessions/<session-id>/summary.md`) are not yet built; the design doc itself notes these depend on the daemon shutdown hook and `loopr sessions end`. Tracked as a follow-up alongside the wiring.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
