# Design Document: Tracing Migration

**Author:** Scott A. Idler
**Date:** 2026-04-06
**Status:** Draft
**Review Passes Completed:** 4/5

## Summary

Replace `log` + `env_logger` with `tracing` + `tracing-subscriber` + `tracing-appender`. Add `#[instrument]` to async entry points so function names appear automatically in log output without manual string prefixes. Preserve all existing log path conventions, level resolution, and format.

## Problem Statement

### Background

Loopr uses `log` + `env_logger` initialized in `setup_logging()` (`src/lib.rs`). Log output goes to `~/.local/share/loopr/sessions/{session_id}/loopr.log` for the daemon or `~/.local/share/loopr/logs/loopr.log` for TUI/CLI. Level is resolved by: CLI flag > `LOG_LEVEL` env var > `config.log_level` > `Info`.

### Problem

Every log call that wants to identify its source function must manually embed the function name in the format string:

```rust
debug!("call_llm_for_children: response stop_reason={:?} content_len={}", ...);
warn!("call_llm_for_children: model did not use tool, falling back to text parsing");
info!("preflight_ac_check(work_id={}): response={:?}", work_id, text);
```

This is error-prone (names go stale on rename, copy-paste introduces wrong names) and adds visual noise to every call site. The heaviest files - `context.rs` (48 log calls), `decomposer.rs` (36), `lifecycle.rs` (31) - all do this manually.

### Goals

- Eliminate manual function-name prefixes in log calls at instrumented entry points
- Preserve existing log file paths, session directory structure, and `latest` symlink
- Preserve existing level resolution (CLI > env > config > default)
- Zero regression on observable log output content
- Leave the door open for structured logging fields without requiring it now

### Non-Goals

- Structured/JSON log output format (not now)
- Distributed tracing (OpenTelemetry, Jaeger, etc.)
- Converting every log call site immediately - phased migration
- Changing level semantics or adding new levels

## Proposed Solution

### Overview

Three-phase migration:

1. **Infrastructure** - swap the logging backend in `setup_logging()`, preserve everything else
2. **Instrumentation** - add `#[instrument(skip_all)]` to high-value async entry points
3. **Cleanup** - remove manual `fn_name:` prefixes from instrumented functions

### Architecture

`tracing` uses a subscriber/layer architecture. The subscriber is global (set once via `set_global_default`). Layers are composable - a fmt layer writes to the file, a filter layer controls levels.

```
log! macros (existing deps)
        |
tracing_log::LogTracer  <-- bridge: forwards log events into tracing
        |
tracing subscriber (global)
        |
  fmt::layer()           <-- formats events and spans to the NonBlocking writer
        |
  NonBlocking writer     <-- async, buffers writes
        |
  loopr.log              <-- same path as today
```

The `#[instrument(skip_all)]` attribute creates a span named after the function. When events (log calls) occur inside, the span name (= function name) is included automatically in the formatted output.

### Data Model

No domain changes. The only new runtime artifact is a `WorkerGuard` from `tracing-appender::non_blocking` that must stay alive for the process lifetime. It is wrapped in a `LogHandle` struct to avoid binding-name awkwardness at call sites.

```rust
pub struct LogHandle {
    pub log_path: PathBuf,
    pub guard: WorkerGuard,  // must not be dropped; public to satisfy dead_code lint
}
```

### API Design

`setup_logging()` return type changes:

```rust
// Before
pub fn setup_logging(config: &Config, cli_log_level: Option<&str>, session_id: Option<&str>)
    -> eyre::Result<PathBuf>

// After
pub fn setup_logging(config: &Config, cli_log_level: Option<&str>, session_id: Option<&str>)
    -> eyre::Result<LogHandle>
```

There are four call sites that need updating. In each case, bind the result to a named variable so the `WorkerGuard` inside `LogHandle` stays alive for the process lifetime:

| Location | Uses `log_path`? | Pattern |
|----------|-----------------|---------|
| `main.rs:28` foreground daemon | Yes | `let log_handle = setup_logging(...)?;` then `log_handle.log_path.parent()...` |
| `main.rs:43` TUI path | No | `let log_handle = setup_logging(...)?;` - binding lives until match arm exits (= program exit) |
| `main.rs:60` CLI path | No | Same as TUI path |
| `daemon.rs:139` grandchild | Yes | `match` returns `LogHandle`; bind to `log_handle`, use `log_handle.log_path` |

For the TUI and CLI paths the `LogHandle` binding must survive until end of `main`. Since `main` returns after `block_on` completes, a named binding at the start of the match arm lives until the arm exits (which is program exit).

For the grandchild `match` block in `daemon.rs`, the update:

```rust
// Before
let log_path = match crate::setup_logging(config, log_level, Some(&session_id)) {
    Ok(p) => p,
    Err(e) => { eprintln!(...); std::process::exit(1); }
};
let session_dir = log_path.parent()...;

// After
let log_handle = match crate::setup_logging(config, log_level, Some(&session_id)) {
    Ok(h) => h,
    Err(e) => { eprintln!(...); std::process::exit(1); }
};
let session_dir = log_handle.log_path.parent()...;
```

The `log_handle` binding lives until the end of `ensure_daemon`, which is the daemon's entire lifetime. Guard is alive throughout.

Instrumentation is additive - no existing function signatures change.

### Implementation Plan

#### Phase 1 - Infrastructure

1. `cargo add tracing`
2. `cargo add tracing-subscriber --features env-filter,fmt`
3. `cargo add tracing-appender tracing-log`
4. In `setup_logging()` (`src/lib.rs`):
   - Keep all path construction, dir creation, and symlink logic unchanged
   - Remove `env_logger::Builder` block
   - Build the file handle identically (`OpenOptions::append`)
   - Call `tracing_log::LogTracer::init().ok()` - bridges `log` events from dependencies into tracing
   - Wrap the file handle: `let (non_blocking, guard) = tracing_appender::non_blocking(file)`
   - Translate the resolved `log::LevelFilter` to `tracing_subscriber::filter::LevelFilter`
   - Build subscriber: `registry().with(fmt::layer().with_writer(non_blocking)).with(level_filter)`
   - Call `tracing::subscriber::set_global_default(subscriber).ok()` (`.ok()` prevents panic if called twice in tests)
   - Return `Ok(LogHandle { log_path: log_file, guard })`
5. Update all four call sites in `main.rs` and `daemon.rs` per the table above
6. Switch `use log::...` imports throughout the codebase to `use tracing::...` (same macro names: `debug!`, `info!`, `warn!`, `error!`, `trace!`). There are 53 files with these imports - this is a mechanical `sed`/replace-all pass, not a logic change. Existing format-string call sites require no further changes.
7. In `setup_logging()`, translate `log::LevelFilter` (returned by `resolve_log_level`) to `tracing_subscriber::filter::LevelFilter` using a match arm. Keep `resolve_log_level()` returning `log::LevelFilter` - it is pure logic unrelated to the backend and has existing unit tests.
8. Remove `env_logger` from `[dependencies]` in `Cargo.toml` via `cargo remove env_logger`. Keep the `log` crate in `[dependencies]` only if `log::LevelFilter` is still referenced publicly; otherwise remove it too. The `tracing-log` bridge ensures dep crates using `log` still work.

#### Phase 2 - Instrumentation

Add `#[instrument(skip_all)]` to async entry points with the most log calls:

- `decomposer`: `call_llm_for_children`, `decompose`, and the tool-use helper
- `lifecycle`: executor lifecycle handlers
- `context`: coordinator context assembly
- `daemon/handlers/*`: each handler function
- `agents/llm_client`: `call`, `call_streaming`

Use `skip_all` by default. Add named fields only when they aid debugging:
```rust
#[instrument(skip_all, fields(work_id = %work_id))]
```

Do NOT instrument:
- Per-item loops (per-tick, per-record validators)
- Sync `ureq`-based validator calls
- Test utilities

#### Phase 3 - Cleanup

For each instrumented function, remove the `fn_name:` prefix from log call format strings. The span name now carries that context. Before:

```rust
debug!("call_llm_for_children: response stop_reason={:?}", reason);
```

After:

```rust
debug!(stop_reason = ?reason, "response");
// or if keeping flat strings:
debug!("response stop_reason={:?}", reason);
```

## Alternatives Considered

### Alternative 1: `function-name` crate + wrapper macros

- **Description:** Add `#[named]` attribute to each function, define `dfn!`/`wfn!` wrapper macros that inject `function_name!()` as a prefix. No change to the logging backend.
- **Pros:** Zero infrastructure change. Drop-in. No return type change.
- **Cons:** Two attributes required per function (`#[named]` + new macro at call site). No path to structured fields. Still requires call-site changes everywhere.
- **Why not chosen:** Same call-site work as `#[instrument]` but fewer long-term benefits.

### Alternative 2: Keep manual prefixes

- **Description:** Continue the current `fn_name:` convention, improve discipline around it.
- **Pros:** No migration work.
- **Cons:** Names go stale silently. Copy-paste errors are already present. Technical debt compounds with every new function.
- **Why not chosen:** The problem exists today and gets worse as the codebase grows.

## Technical Considerations

### Dependencies

New crates:
- `tracing` - span/event API, re-exports `debug!`, `warn!`, etc.
- `tracing-subscriber` - fmt layer, env-filter, registry
- `tracing-appender` - non-blocking file writer, `WorkerGuard`
- `tracing-log` - bridge: forwards `log` crate events into tracing subscriber

### Performance

- `#[instrument]` allocates a `Span` per call. Negligible for async LLM callers (network dominates).
- Do not instrument tight loops or per-item validation.
- Enable `tracing` compile-time level gate. After `cargo add tracing`, edit `Cargo.toml` to add features:
  ```toml
  tracing = { version = "...", features = ["max_level_debug", "release_max_level_info"] }
  ```
  In release builds, `debug!` and `trace!` spans compile to nothing - zero runtime cost.

### Security

No change. Log output goes to the same file-system paths with the same permissions.

### Testing Strategy

- `log_level_tests` in `lib.rs` test `resolve_log_level_from` which is pure - no changes needed there.
- `setup_logging()` itself is not directly unit-tested (it sets global state). No change needed.
- After Phase 1, run `otto ci` to verify compile and existing tests pass.
- Manual smoke test: `loopr --log-level debug status` and verify log file has expected output.
- After Phase 3, verify log lines for instrumented functions show the function name in the span field.

### Rollout Plan

Each phase is a standalone commit/PR. Phase 1 is the only breaking change (return type). Phases 2 and 3 are purely additive/cosmetic.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `WorkerGuard` dropped early, silent log loss | Med | Med | Return guard from `setup_logging`, document in code comment |
| `set_global_default` panics on double-init in tests | Med | Low | Use `.ok()` on init call; test harness uses `try_init` pattern |
| Format regression (different field layout) | Low | Low | Verify format in Phase 1 smoke test before proceeding |
| `log` bridge misses events from deps | Low | Low | `LogTracer::init()` covers all `log` crate users globally |
| Phase 3 cleanup misses a manual prefix | Low | Low | Grep for `fn_name:` pattern after each file is cleaned |

## Open Questions

- [ ] Should the fmt layer emit the span (function) name in its own column or inline? `tracing_subscriber::fmt` default format is `timestamp level span{fields} message`. Decide in Phase 1 before writing log parsers or tooling against the format.
- [ ] Should `fields(work_id = %work_id)` be standardized across all handler instrumentation, or only added where it's already part of the manual prefix?
- [ ] After Phase 1, should `resolve_log_level()` return `tracing_subscriber::filter::LevelFilter` instead of `log::LevelFilter`? This would remove the translation step inside `setup_logging` but requires updating the existing unit tests and may break callers that import `log::LevelFilter`. Defer to implementation.

## References

- `src/lib.rs` - current `setup_logging()` implementation
- `src/main.rs` - call sites (lines 28, 43, 60)
- `src/daemon.rs` - grandchild call site (line 139)
- `src/decomposer.rs` - heaviest manual-prefix file (36 log calls)
- `tracing` docs: https://docs.rs/tracing
- `tracing-appender` docs: https://docs.rs/tracing-appender
