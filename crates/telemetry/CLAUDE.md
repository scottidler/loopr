# telemetry

Observability for v5. First-class concern; owns `tracing` subscriber composition, log layout on disk, run-id generation, span naming conventions, and the back-end for `loopr logs` CLI subcommands.

## In scope

- **Subscriber init.** Compose `tracing-subscriber` layers: JSON file writer to `.loopr/runs/<run-id>/events.log`, pretty file writer to `.loopr/runs/<run-id>/loopr.log`, console mirror at INFO+ for interactive runs. One `init()` entry point called from the binary.
- **Run identifiers.** Generate `YYYYMMDD-HHMMSS` local-time IDs with `-N` collision suffix (atomic; the daemon is the allocator). Expose as a typed `RunId` newtype so callers can't fat-finger a plain `String`.
- **Span naming conventions.** Stable names: `stage.<name>`, `ralph.<role>`, `tool.<name>`. Every span carries `run_id`; nested spans inherit and add `plan_id` / `work_id` when entering their scope. Provide helper macros if they reduce repetition meaningfully; otherwise raw `#[tracing::instrument]` with structured fields.
- **Per-Work fanout subscriber** (built Stage 2; activates Stage 7). Watches the `work_id` span, splits events into `.loopr/runs/<run-id>/work/<work-id>.log`. `WorkFanoutLayer` ships in Stage 2 and runs inert until Stage 7 emits the first `work_id`-bearing span.
- **Log-query helpers.** Back-end functions for `loopr logs tail`, `loopr logs work <id>`, `loopr logs run <id>`. The CLI surface lives in `loopr`; the actual log reading and filtering lives here.

## Out of scope

- **Metrics and tracing export** (OpenTelemetry, Prometheus, OTLP). Not first-gate. If it lands later, it extends this crate; don't speculate yet.
- **LLM call logging per se.** LLM call logs are just `tracing` events emitted by the `llm` crate; this crate doesn't know what an LLM is.
- **Permission/audit events.** Those belong wherever the permission decisions are made; this crate just gives them somewhere to go.
- **TUI rendering.** The TUI, when it lands, subscribes to the same event stream this crate produces; it does not live here.

## Rule

This crate must compile without `tokio`, `reqwest`, or any LLM/network dependency. Tracing subscribers are themselves sync or use their own runtime internals; observability code must not couple to the daemon's async runtime.

The v3/v4 lesson that motivates this crate: observability bolted on late is observability with gaps. Debugging a ralph loop that stalls across three stages required reading three log files and mentally reconstructing causality. Per-run + per-Work fanout, span context, and a typed `RunId` are the minimum to make "follow one Work through every stage" a grep-and-read, not a reconstruction.

## Dependencies

`tracing`, `tracing-subscriber` (with `json` and `env-filter` features), `tracing-appender` (non-blocking file writer), `chrono` (for run-id formatting), `serde` + `serde_json` (for structured event emission). Added via `cargo add` at the time the first code needs them, not speculatively.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape, Observability section
- [../../docs/roadmap.md](../../docs/roadmap.md): Stage 2 is where this crate's first design docs get written
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
