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

## Instrumentation (client side)

The client-side IPC and CLI bodies open spans so a `loopr <subcommand>` invocation produces a self-describing trace under its own per-process run dir:

- `client.connect_or_wait`, `client.connect`, `client.handshake`, `client.request` — debug/info+err with socket path / method / session_id.
- `client.plan_command`, `client.list`, `client.show` — info+err with `target`, `subcommand`, and the command-specific payload (`goal_len`, `kind`, `record_id`).

The handshake span carries the caller's `session_id` so the daemon-side `ipc.connection` span records it as `client_session_id` (see Phase 6 of the layout doc). That is the correlation key for stitching client-side and daemon-side logs of the same request.

## Instrumentation (daemon side)

The daemon's IPC dispatch and pipeline-spawn methods carry `#[tracing::instrument]`:

- `ipc.dispatch` (info) — every request, fields(`request_id`, `method`, `handshake_state`).
- `ipc.handshake` is recorded inline; `ipc.plan_create` (info, `goal_len`, post-record `plan_id`), `ipc.record_list` (debug, `kind`), `ipc.record_get` (debug, `record_id`), `ipc.status` (debug).
- `daemon.run_active`, `daemon.build_context`, `daemon.serve_core` — info+err with `target`/`session_id`/`process_id`/`target_slug`/`pid` so a daemon-startup failure carries every identifier needed to correlate against XDG paths.
- `daemon.reconcile`, `daemon.sweep_worktrees` — info/debug+err.
- `daemon.spawn_implementer_for_work`, `daemon.spawn_reviewer_for_bundle`, `daemon.spawn_integrator_for_bundle` — info, work_id/bundle_id + status fields. Every event emitted while these run inherits the relevant ids automatically.

Per-request scope fields available on the daemon's `ipc.connection` span (set in transport/server.rs at handshake completion): `client_session_id` and `request_id` propagate from the connection span downward into every handler's span.

## Transcripts

The LLM round-trip transcript writers live in `crates/telemetry/src/transcript/` (moved from this crate on 2026-04-24 so agents/decomposer can depend on them — `loopr` is the binary crate and cannot be a dependency of library crates). The agents and decomposer crates wire `append_iteration` calls themselves; this crate no longer has a transcript module. Layout:

- `model.rs` — `TranscriptIteration` struct (model, started_at, latency_ms, prompt/completion tokens, session/process ids, events.log path, system prompt, user prompt, response, parsed actions, dispatcher outcomes, lifeguard decision).
- `render.rs` — `render_iteration(&TranscriptIteration) -> String` produces the markdown block; `redact_paths(text, &[String]) -> String` replaces lines containing any deny-pattern substring with `[redacted: pattern=<p>]`. Per-section cap: `ITERATION_BYTE_CAP / 4` (25 KB), enforced at render time. Truncation marker is the literal `>[truncated: N KB original; sha=<8>]<` from the design doc Q5.
- `mod.rs` — `append_iteration(path, &iter)` opens the file with `create+append`, writes the rendered block, and emits a `tracing::debug!("transcript_appended", path, iteration, bytes)` event after each append.

Paths under `<target>/.loopr/records/`:

- Decomposer: `plans/<plan-id>/decomposition.md`
- Implementer: `works/<work-id>/transcript.md` (append-only across iterations)
- Reviewer: `bundles/<bundle-id>/review.md`

`.git/info/exclude` is updated by `worktree::ensure_loopr_excludes` to cover `.loopr/records/`. Transcripts never get committed.

**Wiring status:** `agents::implementer::run_implementer`, `agents::reviewer::run_reviewer`, AND `decomposer::decompose` all construct a `TranscriptIteration` and call `append_iteration` at every iteration / return-path. Decomposer transcripts land at `plans/<plan-id>/decomposition.md` (one block per LLM call; success + every validation-error variant + retry path + final llm-failed). Failures emit a `warn!` and the agent/decomposer continues.

**System-prompt elision via Anthropic prompt caching:** the system block now ships with `cache_control: { "type": "ephemeral" }`. Cache-creation tokens land on the first call; cache-read tokens on subsequent matching calls. Both surfaces (`llm.anthropic` and `llm.anthropic.free` spans, plus the `llm.anthropic.cache` debug event) record `cache_creation_input_tokens`, `cache_read_input_tokens`, and `cache_hit_ratio`. Below-threshold prompts no-op silently — Anthropic charges and reads the cache only above 1024 tokens (Haiku) / 2048 tokens (Sonnet+Opus 4.x).

## Summary generators

Per-record markdown digests live under `<target>/.loopr/records/<kind>/<id>/summary.md`. The renderers in `crates/loopr/src/summary/` are pure (input record + extra context = `String`); the writers atomic (write-to-temp + rename). Renderers covered: `render_bundle`, `render_work`, `render_plan(plan, &[Work])`. Each has a unit test asserting required sections + post-write file existence.

Best-effort `write_<kind>_summary_best_effort` helpers in `crates/loopr/src/daemon/context.rs` log a `warn!` on failure and return; transcripts and FSM transitions never propagate a summary error.

**Wiring status:** `WorkUpdateSink` and `PlanUpdateSink` ship alongside `BundleUpdateSink`. The `SummaryFanout<S>` decorator (in `crates/loopr/src/daemon/summary_fanout.rs`) implements all three sink traits and writes the matching `summary.md` after every successful inner update. Per option (c-extended) the Work-update path also re-renders the parent Plan summary so child-Work transitions are reflected without waiting on a Plan-level FSM change. `DaemonContext` constructs the fanout in `new()` and threads `&*self.summary_fanout` into every `transition_and_persist_*` call site plus the `IntegratorDeps::bundle_sink` and `ReviewerDeps::store` injections.

**Process / session digests** ship in `crates/telemetry/src/digest/`. `ProcessSnapshot` accumulates per-process counters (records, Bundle/Tick lifecycle, LLM calls + tokens + cost via `MeteredLlmClient<L>`, escalations, corruption_count). `serve_core` writes `runs/<pid>/summary.md` at graceful exit; `loopr sessions end` aggregates every per-process digest under the session and writes `sessions/<sid>/summary.md`. Both renderers emit YAML frontmatter (machine-parseable for the session aggregator) plus a markdown body. Abnormal-exit handling (panic hook + SIGQUIT) is a follow-up; the graceful-exit path is wired.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
