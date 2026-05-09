# telemetry

Observability for v5. First-class concern; owns `tracing` subscriber composition, log layout on disk, session + process + target-slug id generation, span naming conventions, and the back-end for `loopr logs` CLI subcommands.

## In scope

- **Subscriber init.** Compose `tracing-subscriber` layers: JSON file writer to `$XDG_DATA_HOME/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/events.log`, pretty file writer to the sibling `loopr.log`, console mirror at INFO+ for interactive runs, plus the two fanout layers (per-Work + per-Session). One `init(target, session_id, target_slug, process_id, directive)` entry point called from the binary. Runtime process-level state lives in XDG; `<target>/.loopr/` stays small.
- **Identifier taxonomy.** `SessionId` — `YYYYMMDD-HHMMSS[-N]`, atomic via `create_dir` EEXIST on the XDG sessions root; one per user-initiated work session, resumable. `ProcessId` — `pc-<6-char>` random slug; one per OS process (daemon boot, CLI invocation). `target_slug` — claude-style path slugification for the target. All three are typed newtypes with serde transparency so callers can't pass a plain `String`.
- **XDG resolver.** `xdg_root()`, `session_dir()`, `session_target_dir()`, `session_run_dir()` compose the on-disk layout above `$XDG_DATA_HOME/loopr/`.
- **Span naming conventions.** Stable names: `stage.<name>`, `ralph.<role>`, `tool.<name>`. Every span carries `session_id`; nested spans inherit and add `plan_id` / `work_id` when entering their scope. Provide helper macros if they reduce repetition meaningfully; otherwise raw `#[tracing::instrument]` with structured fields.
- **Per-Work fanout subscriber** (built Stage 2; activates Stage 7). Watches the `work_id` span, splits events into `<run-dir>/work/<work-id>.log`. `WorkFanoutLayer` ships in Stage 2 and runs inert until Stage 7 emits the first `work_id`-bearing span.
- **Per-Session fanout subscriber.** Mirror of the Work fanout for session-id routing: events carrying `session_id` or `client_session_id` (daemon's post-handshake field) append to `<xdg>/sessions/<id>/targets/<slug>/session-fanout.log`. LRU-capped writer cache prevents file-handle exhaustion on long-lived daemons handling many sessions.
- **Log-query helpers.** Back-end functions for `loopr logs tail`, `loopr logs runs` (sessions listing under XDG). The CLI surface lives in `loopr`; the actual log reading and filtering lives here.

## Out of scope

- **Metrics and tracing export** (OpenTelemetry, Prometheus, OTLP). Not first-gate. If it lands later, it extends this crate; don't speculate yet.
- **LLM call logging per se.** LLM call logs are just `tracing` events emitted by the `llm` crate; this crate doesn't know what an LLM is.
- **Permission/audit events.** Those belong wherever the permission decisions are made; this crate just gives them somewhere to go.
- **TUI rendering.** The TUI, when it lands, subscribes to the same event stream this crate produces; it does not live here.

## Rule

This crate must compile without `tokio`, `reqwest`, or any LLM/network dependency. Tracing subscribers are themselves sync or use their own runtime internals; observability code must not couple to the daemon's async runtime.

The v3/v4 lesson that motivates this crate: observability bolted on late is observability with gaps. Debugging a ralph loop that stalls across three stages required reading three log files and mentally reconstructing causality. XDG-rooted session layout, per-Work + per-Session fanout, span context, and typed `SessionId` / `ProcessId` are the minimum to make "follow one Work through every stage" a grep-and-read, not a reconstruction.

## Visibility contract (2026-05-09 sweep)

`init_for_test(run_dir, directive) -> TestSubscriberGuard` wraps the
production layer composition (`compose_subscriber`) in a thread-local
`set_default` install. Tests build a tempdir-rooted `events.log`
through the same layers production uses, so a regression in layer
shape or filter wiring fails the contract test, not just an in-memory
fake. The keystone test lives at `tests/events_log_contract.rs`;
per-crate scenarios in other crates' `tests/` use `init_for_test`
directly. Operator grep patterns for the resulting JSONL:
[`docs/telemetry-grep-cookbook.md`](../../docs/telemetry-grep-cookbook.md).

## Dependencies

`tracing`, `tracing-subscriber` (with `json` and `env-filter` features), `tracing-appender` (non-blocking file writer), `chrono` (for session-id formatting), `serde` + `serde_json` (for structured event emission), `dirs` (XDG lookup), `dashmap` (Work fanout cache), `lru` (Session fanout cache). Added via `cargo add` at the time the first code needs them, not speculatively.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape, Observability section
- [../../docs/roadmap.md](../../docs/roadmap.md): Stage 2 is where this crate's first design docs get written
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
