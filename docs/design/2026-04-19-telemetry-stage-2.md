# Design Document: Telemetry Stage 2

**Author:** Scott A. Idler
**Date:** 2026-04-19
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Stand up the `telemetry` crate: `tracing-subscriber` composition with three layers (JSON events, pretty file, console mirror), an atomic `RunId` allocator (`YYYYMMDD-HHMMSS[-N]`), span-naming conventions (`stage.<name>` / `ralph.<role>` / `tool.<name>`), and the log-query back-end for `loopr logs tail` / `loopr logs runs`. Also catches up a Stage 1 gap — the global `--log-level` / `-l` flag that `rules/rust.md` mandates but the CLI skeleton omitted. This is Stage 2 of `docs/roadmap.md`, consolidating the two design docs the roadmap listed (`subscriber-layers.md` and `span-conventions.md`) into one because they cannot be reviewed independently: layers define where spans go, conventions define what the spans look like.

## Problem Statement

### Background

Stage 1 (`2026-04-19-cli-skeleton.md`) gave loopr a real CLI shell with typed stub errors. Every later stage emits log events — the daemon loop, the decomposer, ralph loops, tool invocations, the integrator. Without structured per-run logging in place **before** any stage emits anything, observability drifts the way it did in v3/v4: bolted on late, forever fragmented, forcing "follow one Work through the pipeline" to be a reconstruction exercise across multiple files.

`docs/vision.md` §Observability commits v5 to `tracing` + `tracing-subscriber` (overriding `rules/rust.md`'s `log` + `env_logger` default) and to a three-layer log strategy: JSON events at `.loopr/runs/<run-id>/events.log`, a pretty mirror at `.loopr/runs/<run-id>/loopr.log`, and a console mirror at INFO+ for interactive runs. Stage 2 implements that commitment using blocking `LineWriter<File>` writers (not `tracing-appender::non_blocking` — see §Risks row on the fork trap; `LineWriter`, not `BufWriter` — see the `logs tail` real-time-visibility decision in Open Questions).

### Problem

- No `tracing::Subscriber` is initialized anywhere in the workspace. Every event emitted at Stages 3-9 will vanish until this lands.
- `RunId` is referenced in the vision and in `crates/telemetry/CLAUDE.md` as the newtype that binds per-run state, but it does not exist. The daemon and the CLI need a shared allocator (atomic against collisions when two processes start in the same second).
- Span naming is documented as a convention but not yet a surface — there is no guidance on which spans carry `run_id` vs. `plan_id` vs. `work_id`, and `rules/rust.md` §function-level instrumentation says "every non-trivial function must log its entry at the appropriate level." Stage 2 is where that surface becomes real.
- `loopr logs tail` / `loopr logs runs` are declared in the CLI enum but return `StageUnimplemented { stage: 2 }`. They are part of Stage 2's exit criterion.
- `loopr --log-level debug -C /tmp plan "x"` does not parse — the CLI skeleton did not add the `-l` / `--log-level` flag that `rules/rust.md` mandates. This is a Stage 1 omission that blocks any Stage 2 integration test ("emit a `debug!`, see it land in the pretty log"). Catching it up here.

### Goals

- `telemetry::init(target, run_id, filter: EnvFilter) -> Result<Guard>` composes three blocking tracing layers (JSON events, pretty file, optional console mirror) with `SharedWriter` (newtype wrapping `Arc<Mutex<LineWriter<File>>>`) MakeWriter instances, writes files under `<target>/.loopr/runs/<run-id>/`, and returns a drop-guard whose `Drop` flushes both LineWriters.
- `RunId` is a newtype, atomic allocator, string-roundtrippable. `YYYYMMDD-HHMMSS` local time, `-N` suffix on collision, first run gets the clean name.
- Span naming conventions are codified: helper macros or constants for `stage.<name>` / `ralph.<role>` / `tool.<name>`; documented requirement that `run_id` is on every span, `plan_id` is on spans inside a Plan, `work_id` is on spans inside a Work.
- `loopr logs tail [--lines N]` prints the last `N` lines of the pretty log (`loopr.log`, not `events.log`) for the most recent run at the effective target; `loopr logs runs` lists all run-ids under `.loopr/runs/` with their started-at timestamp, newest first.
- `loopr --log-level debug` / `-l debug` / `LOOPR_LOG_LEVEL=debug` / per-target config set the subscriber's filter level. Precedence follows the vision's chain: CLI > env > config > default (`info`).
- Every invocation that reaches `lib::run` emits a single `loopr.invocation` span at INFO with `run_id` and the parsed subcommand as fields. This span is the Stage 2 smoke-test target.

### Non-Goals

- **Agent-side emission of `work_id`-bearing spans.** The `WorkFanoutLayer` ships in Stage 2, but no Stage 2-6 code emits `work_id`. The first emitter lands in Stage 7's Implementer; the layer stays inert (no per-Work files materialize) until then. This is a *move forward*, not a deferral — Stage 7 no longer has to re-open subscriber composition.
- **OpenTelemetry / OTLP / Prometheus export.** Per `crates/telemetry/CLAUDE.md` §Out of scope.
- **Log rotation / retention / archival.** Runs accumulate under `.loopr/runs/` indefinitely for now; the dir is in `.git/info/exclude`, so it does not pollute git. A future `loopr logs gc` subcommand earns its keep when `.loopr/runs/` starts meaningfully eating disk.
- **TUI log viewer.** `loopr logs tail` is a one-shot print; a scrolling viewer comes with the TUI crate when it lands.
- **Log format stability guarantees.** The JSON schema is whatever `tracing-subscriber::fmt::format::Json` produces in the pinned version; if we upgrade and the schema shifts, old logs still parse, new logs use the new format. The round-trip test pins on the fields we care about (timestamp, level, span name, span fields), not on exact JSON structure.
- **Daemon PID / socket / fork logic.** Stage 4. Stage 2 runs entirely synchronously in the foreground.

### Acceptance Criteria

Each item is an assertable check. The design is Done when every assertion below is true.

- `loopr -C /tmp plan "x"` creates `/tmp/.loopr/runs/<run-id>/` with two files (`events.log`, `loopr.log`), both non-empty, before exiting with `StageUnimplemented`
- `events.log` contains a line parseable as JSON with at least the fields `{ timestamp, level, fields.message, span.name == "loopr.invocation", span.run_id == <run-id> }`
- `loopr.log` contains a human-readable line whose timestamp, level, and span name match the JSON event
- `loopr logs tail -C /tmp` exits 0 and prints the last N lines (default 100) of `loopr.log` from the most recent run under `/tmp/.loopr/runs/`
- `loopr logs runs -C /tmp` exits 0 and prints one run-id per line, newest first, with a human-readable "started at" column
- `loopr logs tail -C /tmp/empty` (no runs) exits non-zero with stderr containing `no runs found`
- `loopr --log-level debug -C /tmp plan "x"` causes a `debug!("loopr::run dispatching ...")` event to appear in both `events.log` and `loopr.log`; `--log-level info` (default) suppresses it
- `LOOPR_LOG_LEVEL=debug loopr -C /tmp plan "x"` behaves the same as the `--log-level debug` CLI flag (env var shadows the default)
- Two `loopr -C /tmp plan "x"` invocations started in the same second produce two distinct run-ids; the first gets `YYYYMMDD-HHMMSS`, the second gets `YYYYMMDD-HHMMSS-2`
- The `Guard` returned by `telemetry::init` flushes all file layers on drop (json + pretty + every cached per-Work fanout writer); verified by test: init → emit → drop guard → read files → assert events present
- Emitting an event inside a span that carries `work_id = "w-00042"` produces a line in `<target>/.loopr/runs/<run-id>/work/w-00042.log` *in addition to* the main `loopr.log`; emitting an event outside any `work_id` scope writes only to the main logs (no spurious per-Work files)
- The `<run-id>/work/` directory is NOT created until the first `work_id`-scoped event fires (in Stage 2 smoke it stays absent; Stage 7 is when it materializes)
- Running inside a pipe (`loopr -C /tmp plan "x" 2>&1 | cat`) suppresses the console layer; running in a real terminal shows it (verified by assertion that the console layer is gated on `std::io::stderr().is_terminal()`)
- `otto ci` at the workspace root is green; `otto ci` inside `crates/telemetry` is green; `otto ci` inside `crates/loopr` is green
- `cargo install --path crates/loopr` produces a binary that reproduces all of the above

## Proposed Solution

### Overview

`crates/telemetry` gains a public surface of four items: `RunId`, `init(...)`, `Guard`, and two log-query functions (`tail_latest_run`, `list_runs`). Span name constants live in the crate root as `pub const STAGE_PREFIX: &str = "stage"; ...` — callers concat with their own labels rather than invoking a helper macro (the helper-macro option is a v2 concern; raw `#[tracing::instrument(name = "stage.plan")]` is readable enough).

`crates/loopr` gains:
1. A global `--log-level` / `-l` flag on `Cli` (catching up a Stage 1 gap).
2. Five lines in `lib::run` that allocate a `RunId`, call `telemetry::init`, emit the `loopr.invocation` span, and hold the `Guard` for the lifetime of the invocation.
3. Real bodies for `LogsCmd::Tail` and `LogsCmd::Runs` that delegate to `telemetry::tail_latest_run` and `telemetry::list_runs`.

The rule-of-thumb separation: **telemetry owns the subscriber, the format, the filenames, and the parse; loopr owns the CLI surface and the invocation lifecycle.**

### Architecture

```
crates/telemetry/
├── src/
│   ├── lib.rs             re-exports + span-name consts + LOG_ENV_VAR const
│   ├── runid.rs           RunId newtype, allocator, FromStr/Display
│   ├── subscriber.rs      init(...) + Guard + SharedWriter
│   ├── fanout.rs          WorkFanoutLayer (per-Work log splitter)
│   ├── query.rs           tail_latest_run, list_runs, RunEntry
│   └── tests.rs           submodule tests per rules/rust.md
└── .otto.yml

crates/loopr/
├── src/
│   ├── cli.rs             + --log-level global flag
│   ├── lib.rs             run() allocates RunId, calls telemetry::init, emits invocation span
│   └── logs.rs            (new) LogsCmd::Tail and ::Runs bodies that call telemetry::
└── ...
```

Per `rules/rust.md`: single-word filenames, Rust 2018+ module style (no `mod.rs`), test bodies in sibling `tests.rs` files.

### Data Model

```rust
// crates/telemetry/src/runid.rs

/// A run identifier in `YYYYMMDD-HHMMSS[-N]` local-time format.
///
/// First run in a given second gets the clean form (`20260419-143012`);
/// subsequent runs in the same second get a disambiguator suffix
/// (`20260419-143012-2`, `20260419-143012-3`, ...).
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Atomically allocate a new RunId by claiming a `.loopr/runs/<id>/` directory
    /// via `std::fs::create_dir`. The EEXIST errno is the collision signal: on
    /// EEXIST we bump the suffix and retry, starting at `-2`. The winning
    /// invocation is the one whose `create_dir` succeeded, which guarantees
    /// atomicity across concurrent loopr processes on the same target.
    pub fn allocate(runs_dir: &Path) -> Result<Self, RunIdAllocError> { ... }

    /// Parse a previously-written RunId string (e.g. from a dir listing).
    /// Validates the `YYYYMMDD-HHMMSS` skeleton and optional `-N` suffix;
    /// rejects anything else.
    pub fn parse(s: &str) -> Result<Self, RunIdParseError> { ... }

    pub fn as_str(&self) -> &str { &self.0 }
}

impl Display for RunId { ... }
impl FromStr for RunId { type Err = RunIdParseError; ... }

#[derive(thiserror::Error, Debug)]
pub enum RunIdParseError {
    #[error("run id `{0}` does not match YYYYMMDD-HHMMSS[-N]")]
    Malformed(String),
}

#[derive(thiserror::Error, Debug)]
pub enum RunIdAllocError {
    /// Allocation retried past the 1000-attempt cap without claiming a free id.
    /// In practice this only fires if the runs directory has ~1000 colliding
    /// ids in the same wall-clock second; treated as unrecoverable.
    #[error("exhausted 1000 allocation retries under {path}")]
    MaxRetries { path: PathBuf },
    /// `create_dir` failed for a reason other than EEXIST (permissions, ENOSPC,
    /// read-only filesystem, ...). Preserved so retry strategies at higher
    /// layers can distinguish "disk full" from "just collided a lot."
    #[error("failed to create run dir {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}
```

```rust
// crates/telemetry/src/subscriber.rs

/// Newtype wrapper that makes `Arc<Mutex<LineWriter<File>>>` usable as
/// a `MakeWriter` for `tracing-subscriber::fmt::Layer`. Required because:
///   - `tracing_subscriber::fmt::MakeWriter` has no blanket impl for `Arc<Mutex<W>>`.
///   - `MutexGuard<'a, W>` does **not** auto-forward the `io::Write` impl of `W`;
///     Rust doesn't forward trait impls through `Deref`. So we also need a
///     `SharedWriterGuard` wrapper whose own `io::Write` impl delegates to
///     the inner guard's `DerefMut` target.
/// Cloneable so the subscriber can hold one copy and the `Guard` another.
#[derive(Clone)]
pub struct SharedWriter(Arc<Mutex<std::io::LineWriter<File>>>);

/// Holds a locked `MutexGuard` and forwards `io::Write` to the inner
/// `LineWriter<File>`. Lives for the duration of one `make_writer()` call
/// (i.e. one `tracing` event emission).
pub struct SharedWriterGuard<'a>(std::sync::MutexGuard<'a, std::io::LineWriter<File>>);

impl<'a> std::io::Write for SharedWriterGuard<'a> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedWriter {
    type Writer = SharedWriterGuard<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        SharedWriterGuard(self.0.lock().expect("log writer mutex poisoned"))
    }
}

/// Drop-guard that flushes the two line-buffered file writers on graceful exit.
///
/// **Why `LineWriter`, not `BufWriter`**: both amortize syscalls by buffering,
/// but `LineWriter` guarantees a flush after every `\n`. This matters for
/// Stage 4 — the daemon is long-lived, so `loopr logs tail` reads the file
/// while the daemon is still writing. `BufWriter` would let up to ~8 KiB of
/// recent events sit in daemon memory invisible to `tail`; `LineWriter`
/// flushes each `tracing` event immediately (every emit ends in `\n`),
/// keeping tailing strictly real-time. The per-line syscall cost is free at
/// v5's volume.
///
/// **Why a Guard at all**: belt-and-suspenders for the graceful-exit path.
/// `LineWriter` already flushes on newline and on its own Drop, but the
/// LineWriter is held by the global subscriber which never gets dropped
/// (globals leak at process exit). The explicit `Guard::drop` reaches
/// through the Arc and flushes the final partial line (if any) before
/// the process exits.
///
/// Signal-interrupted termination (SIGINT/SIGTERM/SIGKILL) still skips
/// Drop; the LineWriter's per-newline flush shrinks the blast radius from
/// ~8 KiB (BufWriter) to at most one in-progress line. That gap is
/// acknowledged in §Risks and deferred to Stage 4's signal handler.
#[must_use = "telemetry::Guard must be held for the lifetime of the invocation; dropping early truncates logs"]
pub struct Guard {
    json_writer: SharedWriter,
    pretty_writer: SharedWriter,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // Lock, flush; ignore errors — this runs on the normal-exit path and
        // failing to flush here is worth a log line but not a panic. A real
        // flush failure means something catastrophic (disk full, etc.) that
        // the invocation has already errored on.
        if let Ok(mut w) = self.json_writer.0.lock() {
            let _ = w.flush();
        }
        if let Ok(mut w) = self.pretty_writer.0.lock() {
            let _ = w.flush();
        }
    }
}

/// Initialize the global tracing subscriber for this process.
///
/// Creates `<target>/.loopr/runs/<run_id>/` if it does not exist, opens
/// `events.log` and `loopr.log` there as blocking `LineWriter<File>` handles
/// wrapped in `Arc<Mutex<_>>` via the `SharedWriter` newtype (the
/// `MakeWriter` shape that `tracing-subscriber` requires), and composes
/// four layers:
///   1. JSON layer    -> events.log (filtered by `filter`)
///   2. Pretty layer  -> loopr.log  (filtered by `filter`)
///   3. Console layer -> stderr     (INFO and above, gated on IsTerminal(stderr))
///   4. Fanout layer  -> work/<work_id>.log per Work-scoped event (see fanout.rs)
///
/// **Writers are blocking, not `tracing-appender::non_blocking`.** The
/// non-blocking appender spawns a worker thread per file; `fork()` destroys
/// all threads except the caller, so initializing non-blocking writers
/// pre-fork in `loopr::run` would produce a daemon child (Stage 4) writing
/// into a dead channel. Blocking writers are fork-safe by construction.
/// Performance is a non-issue at v5's scale (single-digit events/second
/// worst case on a Stage 9 E2E run); `LineWriter` amortizes syscalls.
///
/// **Filter is `EnvFilter`, not `LevelFilter`.** `EnvFilter` accepts both
/// a bare level (`"debug"`) and per-target directives
/// (`"loopr=debug,tools=error,warn"`). This preserves `tracing`'s per-crate
/// filtering capability — the primary reason v5 picked `tracing` over the
/// `log` crate. `LevelFilter` would strip that capability unrecoverably.
///
/// Registers the subscriber globally via `tracing::subscriber::set_global_default`.
/// Callable exactly once per process; a second call returns `AlreadyInitialized`.
pub fn init(
    target: &Path,
    run_id: &RunId,
    filter: tracing_subscriber::EnvFilter,
) -> Result<Guard, TelemetryInitError> { ... }

#[derive(thiserror::Error, Debug)]
pub enum TelemetryInitError {
    #[error("telemetry::init called twice in the same process")]
    AlreadyInitialized,
    #[error("failed to create runs dir {path}: {source}")]
    DirCreate { path: PathBuf, source: std::io::Error },
    #[error("failed to open log file {path}: {source}")]
    FileOpen { path: PathBuf, source: std::io::Error },
}
```

```rust
// crates/telemetry/src/query.rs

/// One entry in `loopr logs runs`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RunEntry {
    pub run_id: RunId,
    pub started_at: chrono::NaiveDateTime,  // parsed back from the run-id
    pub path: PathBuf,                       // .loopr/runs/<id>/
}

/// Read the last `n` lines of `<latest-run>/loopr.log` under `<target>/.loopr/runs/`.
/// Returns `Err(NoRunsFound)` if the runs dir is empty or absent.
pub fn tail_latest_run(target: &Path, n: usize) -> Result<String, QueryError>;

/// List all run-ids under `<target>/.loopr/runs/`, newest first, with their
/// parsed started-at timestamps. Directories whose names do not parse as
/// valid RunIds are skipped silently (not errors — a user might drop stray
/// files there).
pub fn list_runs(target: &Path) -> Result<Vec<RunEntry>, QueryError>;

#[derive(thiserror::Error, Debug)]
pub enum QueryError {
    #[error("no runs found at {path}")]
    NoRunsFound { path: PathBuf },
    #[error("failed to read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}
```

```rust
// crates/telemetry/src/fanout.rs

/// Per-Work log splitter. Implements `tracing_subscriber::Layer` so it
/// can compose alongside the three fmt::Layer instances in `subscriber::init`.
///
/// Behavior: on every event, walks the enclosing span hierarchy looking for
/// a `work_id` field. If found, ensures a file at
/// `<target>/.loopr/runs/<run-id>/work/<work_id>.log` is open and appends
/// the pretty-formatted event to it. File handles are cached in a
/// `DashMap<String, SharedWriter>` so each `work_id` opens exactly once
/// per run.
///
/// In Stage 2 the layer runs harmlessly — no code emits `work_id`-bearing
/// spans yet, so the hot path is just "walk spans, find no work_id, return."
/// Stage 7's first Implementer agent produces `work_id` spans and the layer
/// starts materializing files automatically. No subscriber reconfiguration
/// at any later stage.
pub struct WorkFanoutLayer {
    runs_root: PathBuf,                            // <target>/.loopr/runs/<run-id>/work/
    cache: dashmap::DashMap<String, SharedWriter>, // work_id -> writer
}

impl WorkFanoutLayer {
    /// Create a layer rooted at the current run's work directory.
    /// Creates `<runs_root>/work/` lazily (on first `work_id` sighting),
    /// not in `new()` — keeps the dir empty when there are no Work items.
    pub fn new(run_dir: &Path) -> Self { ... }
}

impl<S> tracing_subscriber::Layer<S> for WorkFanoutLayer
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        // 1. Walk span hierarchy from current span up. Look for a field
        //    named `work_id` on any ancestor.
        // 2. If not found, return (event goes to main layers only).
        // 3. If found, get-or-insert the SharedWriter for that work_id.
        // 4. Render the event using the same pretty format as loopr.log
        //    and write via the cached SharedWriter.
    }
}
```

```rust
// crates/telemetry/src/subscriber.rs — Guard extension

#[must_use = "telemetry::Guard must be held for the lifetime of the invocation; dropping early truncates logs"]
pub struct Guard {
    json_writer: SharedWriter,
    pretty_writer: SharedWriter,
    // NEW: fanout layer's file cache. Guard::drop flushes all per-work files.
    fanout_cache: Arc<dashmap::DashMap<String, SharedWriter>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        // ... existing json/pretty flush ...
        // NEW: flush every cached per-work writer.
        for entry in self.fanout_cache.iter() {
            if let Ok(mut w) = entry.value().0.lock() {
                let _ = w.flush();
            }
        }
    }
}
```

```rust
// crates/telemetry/src/lib.rs (additions)

pub const LOG_ENV_VAR: &str = "LOOPR_LOG_LEVEL";

pub const STAGE_PREFIX: &str  = "stage";   // stage.plan, stage.decompose, ...
pub const RALPH_PREFIX: &str  = "ralph";   // ralph.implementer, ralph.reviewer, ...
pub const TOOL_PREFIX: &str   = "tool";    // tool.bash, tool.edit, ...

// Re-exports
pub use runid::{RunId, RunIdParseError};
pub use subscriber::{init, Guard, TelemetryInitError, SharedWriter, SharedWriterGuard};
pub use fanout::WorkFanoutLayer;
pub use query::{tail_latest_run, list_runs, RunEntry, QueryError};
```

### API Design

#### Telemetry init flow (from `loopr::run`)

```rust
// crates/loopr/src/lib.rs — additions to existing run()

pub fn run(cli: Cli) -> eyre::Result<()> {
    let cwd = std::env::current_dir().wrap_err("current_dir")?;
    let env_target = std::env::var("LOOPR_TARGET").ok();
    let target = target::resolve(cli.chdir.as_deref(), env_target.as_deref(), &cwd)?;
    guard::check(&target)?;

    // Stage 2: telemetry init.
    //
    // The declaration order here is load-bearing. Rust drops locals in
    // reverse-declaration order at scope exit. We want:
    //   1. `enter` (span entered-scope guard) dropped first → exits the span
    //   2. `invocation` (the Span handle itself) dropped next → emits the
    //      span's "close" event into the subscriber, which writes it via
    //      the LineWriter
    //   3. `guard` (telemetry::Guard) dropped last → flushes both LineWriters
    //      so the close event reaches disk before the process exits
    //
    // DO NOT add explicit `drop(enter); drop(guard);` lines before returning —
    // that would flush BEFORE the span close event exists, and the close
    // event would then be written to a buffer that nothing flushes. RAII
    // handles the ordering correctly; manual drops break it.
    //
    // Variables are named (not `_guard` / `_enter`) because `rules/rust.md`
    // forbids the leading-underscore crutch; these locals are used — their
    // liveness (Drop timing) is the entire point.
    let filter = resolve_log_filter(cli.log_level.as_deref())?;
    let runs_dir = target.join(".loopr").join("runs");
    std::fs::create_dir_all(&runs_dir)
        .wrap_err_with(|| format!("create {}", runs_dir.display()))?;
    let run_id = RunId::allocate(&runs_dir)?;
    let guard = telemetry::init(&target, &run_id, filter)?;
    let invocation = tracing::info_span!(
        "loopr.invocation",
        run_id = %run_id,
        subcommand = %cli.command.label(),
    );
    let enter = invocation.enter();

    let result = dispatch(&target, cli.command).map_err(Into::into);
    // Scope end: enter → invocation (close event emitted) → guard (flushes).
    // `result` stays live through the return; locals drop before it.
    result
}

/// Resolve the EnvFilter directive from CLI flag > env var > default.
///
/// Accepts any `EnvFilter`-parseable directive — bare levels (`"debug"`),
/// per-target directives (`"loopr=debug,tools=error"`), or combinations
/// (`"warn,loopr=debug"`). Returning `EnvFilter` directly means the caller
/// does not re-parse and the same directive surface applies whether the
/// user set it on the CLI or via `LOOPR_LOG_LEVEL`.
fn resolve_log_filter(
    flag: Option<&str>,
    // TODO(stage-5): config-file resolution takes a second parameter here
) -> eyre::Result<tracing_subscriber::EnvFilter> {
    let directive = flag
        .map(str::to_owned)
        .or_else(|| std::env::var(telemetry::LOG_ENV_VAR).ok())
        .unwrap_or_else(|| "info".to_string());
    tracing_subscriber::EnvFilter::try_new(&directive)
        .map_err(|e| eyre::eyre!("invalid log filter `{directive}`: {e}"))
}
```

#### `Cli` addition

```rust
// crates/loopr/src/cli.rs — add to the Cli struct

#[derive(Parser, Debug)]
pub struct Cli {
    #[arg(short = 'C', long = "chdir", global = true, value_name = "PATH")]
    pub chdir: Option<PathBuf>,

    /// Set the tracing filter directive. Env: LOOPR_LOG_LEVEL. Default: info.
    ///
    /// Accepts any `EnvFilter`-parseable string:
    ///   --log-level debug                           (bare level, all targets)
    ///   --log-level loopr=debug,tools=error         (per-target directives)
    ///   --log-level warn,loopr::agents=trace        (combination)
    ///   --log-level off                             (silence everything)
    ///
    /// Parsed via `EnvFilter::try_new` at subscriber init — no clap-side
    /// `value_parser` needed. Invalid directives surface as an eyre error
    /// at `lib::run` entry before any log file is created.
    #[arg(short = 'l', long = "log-level", global = true, value_name = "DIRECTIVE")]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}
```

No `parse_log_level` helper — `EnvFilter::try_new` is the parser and it lives in `resolve_log_filter` at the use site, not in the clap struct. Keeps the CLI surface as plain `Option<String>` and defers validation to `telemetry`'s own concerns.

#### `Command::label()` helper

```rust
impl Command {
    /// Stable string label for telemetry / error reporting.
    /// Matches the existing `StageUnimplemented.subcommand` labels
    /// from `2026-04-19-cli-skeleton.md` (Subcommand → Stage table).
    pub fn label(&self) -> &'static str {
        match self {
            Command::Init => "init",
            Command::Plan { .. } => "plan",
            Command::Decompose { .. } => "decompose",
            Command::Execute { .. } => "execute",
            Command::Integrate => "integrate",
            Command::Daemon { cmd } => match cmd {
                DaemonCmd::Start { .. } => "daemon-start",
                DaemonCmd::Stop => "daemon-stop",
                DaemonCmd::Status => "daemon-status",
            },
            Command::Score { .. } => "score",
            Command::Logs { cmd } => match cmd {
                LogsCmd::Tail { .. } => "logs-tail",
                LogsCmd::Runs => "logs-runs",
            },
            Command::List { .. } => "list",
        }
    }
}
```

The existing `StageUnimplemented { subcommand }` field gets fed from this same `label()` — one source of truth for subcommand naming, used in both error messages and span fields.

#### `logs tail` / `logs runs` dispatch

```rust
// crates/loopr/src/logs.rs (new)

pub fn handle_tail(target: &Path, lines: usize) -> Result<(), LooprError> {
    let out = telemetry::tail_latest_run(target, lines)
        .map_err(|e| LooprError::LogsQuery(e.to_string()))?;
    print!("{out}");
    Ok(())
}

pub fn handle_runs(target: &Path) -> Result<(), LooprError> {
    let runs = telemetry::list_runs(target)
        .map_err(|e| LooprError::LogsQuery(e.to_string()))?;
    for r in runs {
        println!("{}  {}", r.run_id, r.started_at.format("%Y-%m-%d %H:%M:%S"));
    }
    Ok(())
}
```

`dispatch()` in `lib.rs` replaces the two `StageUnimplemented` arms for `Logs` with calls to these handlers; the `target` passed in is the same one `lib::run` resolved and used for `telemetry::init`, which means `loopr -C /tmp logs tail` and `loopr -C /tmp plan "x"` see the same `.loopr/runs/` directory.

`LooprError` gains one variant:

```rust
#[error("log query failed: {0}")]
LogsQuery(String),
```

#### Function instrumentation conventions

General `#[tracing::instrument]` usage — the default pattern, `skip_all` + explicit `fields(...)` discipline, level-by-role rules, `%`/`?` formatting, `ret`/`err`, and when NOT to instrument — is documented in `rules/rust.md` §Function-level instrumentation with `tracing`. This subsection pins the **v5-specific** scope fields that every instrumented function in loopr must carry.

**Required fields by scope** (the load-bearing "follow one Work through the pipeline" keys):

- Every function: implicit `run_id` via the span hierarchy (set once in `loopr.invocation`; never re-declared).
- Functions inside a Plan: `plan_id` as a field.
- Functions inside a Work: `work_id` as a field. **Critical for the `WorkFanoutLayer`** — missing `work_id` means the event is NOT routed to the per-Work log file even if the function semantically belongs to a Work.
- Functions inside a tool invocation: `tool` as a field (the tool name string).

An emission site missing its scope key forces log-reconstruction — the v3/v4 failure this crate exists to prevent.

**Enforcement:** no `clippy` lint covers "function should be instrumented." PR review is the gate. If repetition pain justifies it later, `telemetry::span_for_stage!` / `span_for_ralph!` / `span_for_tool!` macros are the earned upgrade path (§Open Questions).

### Implementation Plan

Four phases, mechanical across the board. Stage 2 is wiring, not algorithm design.

#### Phase 1: Telemetry crate public surface

**Model:** sonnet

- `cargo add --package telemetry tracing tracing-subscriber chrono serde serde_json thiserror eyre rev_lines dashmap` (versions flow from `[workspace.dependencies]` where already declared; `thiserror` and `eyre` get pinned via workspace where they aren't yet; `rev_lines` and `dashmap` are new direct deps — see below).
- **Dropped from original draft:** `tracing-appender`. Stage 2 uses blocking `LineWriter<File>` writers (wrapped via a `SharedWriter` newtype) through `tracing-subscriber::fmt::Layer::with_writer`, not the `non_blocking` worker-thread appender. Reason: `fork()` (Stage 4) destroys all threads except the caller; non-blocking appenders pre-fork would produce a daemon child writing into a dead channel. Blocking writers are fork-safe by construction. Stage 4 can revisit if daemon log volume ever forces it, at which point post-fork init is the natural place for non-blocking to appear.
- **New dep: `rev_lines`** (crates.io). Provides `RevLines::new(File)` — an iterator that reads backward line-by-line with correct UTF-8 boundary handling and buffered I/O. Used by `tail_latest_run` to read the last `N` lines of `loopr.log` without slurping the whole file. Picked over `rev_buf_reader` (lower-level, requires manual line-splitting) because we want last-N-lines specifically and `rev_lines`'s iterator composes with `.take(n)` cleanly.
- **New dep: `dashmap`** (crates.io). Used in `fanout.rs` for the `DashMap<String, SharedWriter>` cache of per-Work file handles. Concurrent-safe, lock-free reads on cache hits — important because the Layer's `on_event` fires on every emission and must not become the serialization bottleneck. Alternative `Arc<Mutex<HashMap<...>>>` would force every log emission (even outside Work scope) to contend on one global lock; `DashMap`'s sharded locking avoids that.
- Write `runid.rs`: `RunId` newtype, `allocate(runs_dir)`, `parse`, `Display`, `FromStr`. Use `create_dir` (not `create_dir_all`) for the atomic claim; EEXIST → increment suffix, retry. Cap retries at 1000 (paranoid upper bound; in practice collisions are single-digit).
- Write `subscriber.rs`: `SharedWriter` newtype + `SharedWriterGuard<'a>` wrapper (manual `impl io::Write` delegating to the inner `MutexGuard<LineWriter<File>>` — `MutexGuard` does not auto-forward `Write` through `Deref`, so the wrapper is mandatory for the `MakeWriter::Writer` associated type). `init(target, run_id, filter: EnvFilter) -> Result<Guard>` composes four layers (json, pretty, optional console, fanout) with two `SharedWriter` instances (shared between the subscriber and the returned `Guard`) and a `WorkFanoutLayer` instance whose `DashMap` is also shared with the Guard. `Drop` on `Guard` locks through each Arc and flushes both main LineWriters plus every cached per-Work writer. Console layer composed via `Option<Layer>` — `Some(layer)` if `std::io::stderr().is_terminal()`, else `None`. `set_global_default` with `AlreadyInitialized` mapped to `TelemetryInitError::AlreadyInitialized`.
- Write `fanout.rs`: `WorkFanoutLayer` impl of `tracing_subscriber::Layer<S>`. On `on_event`, walk `ctx.event_scope()` upward looking for a field named `work_id`; on first sighting for a given id, lazily create `<run-dir>/work/` + open `<run-dir>/work/<work_id>.log` as a `LineWriter<File>` wrapped in `SharedWriter`, cache in the DashMap, then append the pretty-formatted event. Cache miss path is slow (mkdir + open); cache hit path is lock-free lookup + write. No-op when no `work_id` field is found in any ancestor span (Stage 2 baseline).
- Write `query.rs`: `tail_latest_run`, `list_runs`, `RunEntry`, `QueryError`. `list_runs` sorts by parsed RunId timestamp descending (not by mtime — run-id parse is source of truth); `tail_latest_run` picks the head and uses `rev_lines::RevLines::new(File::open(...))`. **Error handling is `filter_map(Result::ok)`, not `collect::<Result<_, _>>()`** — a daemon killed mid-write (Stage 4+) can leave a torn UTF-8 sequence at the file tail; yielding an `Err` there must not crash `logs tail`. Skip-on-error lets valid preceding lines reach the user even if the very last byte is torn. Take `n`, collect into `Vec<String>`, reverse for natural display order.
- Re-exports + span-prefix consts + `LOG_ENV_VAR` in `lib.rs`.
- Tests in `crates/telemetry/src/tests.rs` (sibling submodule, per `rules/rust.md`):
    - `runid`: allocate into an empty dir (claims `YYYYMMDD-HHMMSS`); allocate twice in quick succession (second gets `-2`); allocate into a dir pre-populated with collision victims up to `-5` (sixth allocation gets `-6`); parse valid and invalid strings.
    - `subscriber`: init + emit one event + drop guard + parse `events.log` + assert fields. Init twice → `AlreadyInitialized`. (Note: `set_global_default` is process-global; to keep tests independent, each test runs via `tracing::subscriber::with_default` on a local subscriber built by the same composition logic, with a thin `compose()` helper extracted. The global `init()` is exercised via the smoke test in Phase 4.)
    - `fanout`: compose a local subscriber with `WorkFanoutLayer`; emit one event inside `info_span!("stage.x", work_id = "w-test-01").in_scope(|| ...)`; assert `<run>/work/w-test-01.log` exists and contains the event. Emit one event outside any work_id span; assert no new work file is created. Emit two events with the same work_id; assert the same file is reused (cache hit). Emit events with two different work_ids; assert two files exist.
    - `query`: temp dir with two run subdirs, `list_runs` returns them newest-first; `tail_latest_run` with N=5 on a pretty log with 20 lines returns the last 5; `NoRunsFound` on an empty parent.
- `otto ci` inside `crates/telemetry` green.

#### Phase 2: `--log-level` global flag (Stage 1 catch-up)

**Model:** sonnet

- Add `--log-level` / `-l` to the `Cli` struct as shown in "API Design" — `Option<String>`, global, no `value_parser`. Global so subcommands inherit it.
- Add a `Command::label()` impl on the existing enum, one arm per variant. Existing tests that match on `StageUnimplemented { subcommand }` switch to calling `cli.command.label()` where they currently hardcode strings — one source of truth.
- Update `crates/loopr/docs/design/2026-04-19-cli-skeleton.md` with a single-line "amended by 2026-04-19-stage-2" annotation under its Status heading. Do not rewrite the Stage 1 doc; pin the amendment.
- Update roadmap Stage 1 line to note `--log-level` is under Stage 2. The roadmap edit is one-liner.
- Clap tests: `--log-level debug` produces `Some("debug".to_string())`; `-l "loopr=debug,tools=error"` produces the directive string verbatim; omitted is `None` (caller resolves default). EnvFilter parse-correctness is tested at the `resolve_log_filter` unit boundary, not at the clap boundary.
- `cargo test -p loopr` green.

#### Phase 3: Wire telemetry into `lib::run`

**Model:** sonnet

- Add `telemetry` path dep is already declared in `crates/loopr/Cargo.toml`; it just does not yet get imported. Stage 2 flips that to a real use.
- `resolve_log_filter(flag: Option<&str>) -> eyre::Result<EnvFilter>`: the precedence chain from the goals section (CLI flag > `LOOPR_LOG_LEVEL` env > `"info"` default). Invalid directive strings surface as an eyre error before any log file is created. Config-file input is a Stage 5 concern; pin a `// TODO(stage-5): config` comment and leave the function's signature stable so Stage 5 adds a parameter without touching callers.
- `run()`: allocate RunId → create `.loopr/runs/` if missing → `telemetry::init` → build `loopr.invocation` span → enter it → dispatch. Named `guard` / `enter` (not `_guard` / `_enter`) per the exemption decision in Open Questions. **No explicit `drop()` calls** — let RAII handle scope-exit ordering. Declaration order is `guard` before `invocation` before `enter`, so reverse-drop order is `enter` (span exit) → `invocation` (emits close event into the subscriber) → `guard` (flushes LineWriters). Explicit drops would flush before the close event exists, losing it.
- **`dispatch` signature change.** Stage 1's `dispatch` is `fn dispatch(command: Command) -> Result<(), LooprError>`. Stage 2 extends it to `fn dispatch(target: &Path, command: Command) -> Result<(), LooprError>` so the `Logs { cmd: Tail | Runs }` arms can call `logs::handle_tail(target, lines)` / `logs::handle_runs(target)`. The other stub arms ignore `target` for now; they'll use it from Stage 4 onward. This is a local, non-breaking change — `dispatch` is crate-private.
- Replace the two `Logs { cmd: Tail | Runs }` `StageUnimplemented` arms in `dispatch` with calls to `logs::handle_tail` / `logs::handle_runs` (from `crates/loopr/src/logs.rs`, new).
- `LooprError::LogsQuery(String)` variant added to `error.rs`.
- `cargo check -p loopr` green, then `cargo test -p loopr` green.

#### Phase 4: Exit-criterion smoke test

**Model:** sonnet

- Extend `crates/loopr/tests/smoke.rs` (Stage 1's integration test file) with:
    - `logs_tail_reads_pretty_from_latest_run` — `loopr -C <tempdir> plan "x"` (returns StageUnimplemented; writes logs before dispatch fails), then `loopr -C <tempdir> logs tail --lines 10` prints non-empty output; parse first line for timestamp/level.
    - `logs_runs_lists_newest_first` — two plan invocations back-to-back, `logs runs` prints two lines newest-first, first column is a valid RunId.
    - `events_log_is_valid_json` — same tempdir after a plan invocation; read `events.log`, `serde_json::from_str` every line, assert at least one has `span.name == "loopr.invocation"` with the expected `run_id`.
    - `log_level_gate_works` — with `--log-level info`, a debug event emitted in `lib::run` does NOT appear in `loopr.log`; with `--log-level debug`, it does.
    - `console_gated_on_tty` — `assert_cmd` runs with a piped stderr, assert the invocation span does NOT appear on stderr (piped ≠ TTY). Running with a real PTY is out of scope for Stage 2; a `#[ignore]` manual-smoke test documents how to verify interactively.
    - `runid_collision_allocates_disambiguator` — two `RunId::allocate` calls into the same empty dir within one second (same process, quick succession) produce distinct ids where the second has a `-N` suffix. This is a unit test in `crates/telemetry/src/tests.rs`, not a binary smoke, because mocking `chrono::Local::now` cleanly is easier in-process.
- `otto ci` at workspace root green.
- Bump to `v0.5.2` on the `v5` branch once Phases 1–4 are all green (same override of `rules/git.md` as cli-skeleton).

### Phase Model Summary

All four phases are mechanical: subscriber composition follows the `tracing-subscriber` docs, the run-id allocator is a loop with `create_dir`, the CLI addition is one clap field, the smoke tests are `assert_cmd` invocations. Sonnet throughout. No phase is subtle enough to earn Opus.

## Alternatives Considered

### Alternative 1: Split into two design docs as the roadmap originally listed

- **Description:** Write `subscriber-layers.md` and `span-conventions.md` as separate docs.
- **Pros:** Follows the roadmap literally; smaller documents are easier to grep.
- **Cons:** The two cannot be reviewed independently — layers define the file destinations, conventions define what the files contain. Any review of "are the span names right" has to reference the subscriber design to know the spans are actually routed somewhere. Splitting forces cross-doc pointers for every non-trivial question.
- **Why not chosen:** Same crate, same motivating stage, same exit criterion. The roadmap's two-bullet listing is a hint, not a mandate (the roadmap preamble explicitly says "design docs inside each stage are placeholders until their stage's time comes; actual content is written motivated by the failing run that stage exists to fix"). Consolidation is within scope of that rule. The roadmap entry gets amended alongside this doc landing.

### Alternative 2: Use `env_logger` (per `rules/rust.md` default) instead of `tracing`

- **Description:** Simpler stack; one writer; no span hierarchy.
- **Pros:** Familiar; fewer dependencies; easier in-memory tests.
- **Cons:** A multi-crate async daemon with long-lived stages (Stages 4-9) needs span context that survives across tasks and crate boundaries. Without spans, "follow one Work from Plan through Tick" requires correlating `run_id` / `plan_id` / `work_id` by grep on flat events — exactly the failure mode v3/v4 produced and that `crates/telemetry/CLAUDE.md` names as the motivating lesson.
- **Why not chosen:** `docs/vision.md` §Observability committed to `tracing` explicitly for this reason. The Stage 2 doc reaffirms that choice; it does not re-open it. `rules/rust.md` §Observability allows project-level overrides when justified, and the vision documents the justification.

### Alternative 3: Initialize telemetry before clap runs

- **Description:** Swap the order in `main.rs` so `telemetry::init` fires before `Cli::parse`. Then `loopr --version` and `loopr --help` would also emit an invocation span, satisfying the roadmap's literal exit criterion.
- **Pros:** Matches the roadmap wording verbatim.
- **Cons:** Every `--version` / `--help` call allocates a `.loopr/runs/<run-id>/` directory, writes two log files, and leaves them behind. `--version` / `--help` are metadata queries; they should not spawn filesystem state. On an uninitialized target (`-C /does-not-exist`), init would fail just to print a version string — regression in usability.
- **Why not chosen:** The roadmap's exit criterion as literally written is unsatisfiable (clap short-circuits `--version`) and its intent is clearly "prove the subscriber works end-to-end," which `loopr -C /tmp plan "x"` proves equally well without side-effect regression. The roadmap entry gets amended to replace the `--version` wording with "any invocation that reaches `lib::run`."

### Alternative 4: Atomic RunId allocation via lockfile instead of `create_dir`

- **Description:** Take a flock on `.loopr/runs/.lock` before allocating; inside the lock, pick the next free id by dir-listing.
- **Pros:** Works even if the filesystem is one where directory create is not atomic (network filesystems, some FUSE).
- **Cons:** Adds a dependency (`fs2` or `nix`); lockfile cleanup on crash is its own thornbush; local filesystems (ext4, APFS, NTFS) all provide `mkdir` atomicity already and that's the only filesystem Stage 2 has to work on. `taskstore` itself makes the same assumption.
- **Why not chosen:** `create_dir` returning EEXIST is the atomicity primitive, inherited from the kernel, free of dependencies. If network filesystems bite a future user, we revisit.

### Alternative 5: Store run-ids as numeric monotonic counters instead of timestamps

- **Description:** `runs/1/`, `runs/2/`, `runs/3/`. Atomic allocation via `create_dir` on the next integer.
- **Pros:** No chrono dependency in the allocator; conceptually simpler; no collision suffixes.
- **Cons:** Loses the at-a-glance "when did this run happen" from `ls .loopr/runs/`. Requires a separate mtime or metadata read to get the timestamp. Timestamp-based ids are self-documenting and v4's chosen form; no prior-run logs need migration (v5 is a clean break) but preserving the format keeps muscle memory and tooling intuitions intact.
- **Why not chosen:** Readability wins. `20260419-143012-2` tells you the wall-clock time at a glance; `74` does not. Collision suffixes are rare and numeric disambiguation when they do happen is fine.

## Technical Considerations

### Dependencies

**Telemetry crate (first real deps):**

- `tracing` — workspace dep, already declared.
- `tracing-subscriber` — workspace dep, already declared with `json` + `env-filter` features.
- `chrono` — workspace dep, already declared (`serde` feature).
- `serde` / `serde_json` — workspace deps, already declared. Used for the round-trip test and for `RunEntry` serialization.
- `thiserror` — added per-crate via `cargo add` (same treatment as `loopr`); telemetry is a library crate, thiserror is correct.
- `eyre` — workspace dep; used sparingly in `RunId::allocate` where the error is a one-shot top-level call (`loopr::run` maps it).
- `rev_lines` — **new direct dep** added via `cargo add rev_lines` at the telemetry crate. UTF-8-correct backward line iterator for `tail_latest_run`.
- `dashmap` — **new direct dep** added via `cargo add dashmap`. Concurrent-safe sharded hashmap used by `WorkFanoutLayer` to cache per-Work file handles without serializing every log emission on a global Mutex.

**Not used:** `tracing-appender`. Originally in the draft; removed in favor of blocking `LineWriter<File>` writers (wrapped in `Arc<Mutex<_>>` via a `SharedWriter` newtype) to dodge the `fork()` thread-destruction trap (see §Risks row on the fork trap). The workspace still declares `tracing-appender` in `[workspace.dependencies]` for Stage 4+ if the daemon child's log-volume ever justifies non-blocking writes post-fork.

**Loopr crate (additions):**

- No new external deps; `telemetry` path dep is already declared. `assert_cmd` / `predicates` / `tempfile` already present from Stage 1 smoke tests.

No `tokio`, no `reqwest`, no LLM deps enter the telemetry crate — matches the `crates/telemetry/CLAUDE.md` rule.

### Performance

- **Hot path cost of emitting a span**: blocking writer with `LineWriter` amortization is a `Mutex` lock + in-memory write + a syscall on the trailing `\n`. Acceptable even in tight loops; expensive loops that emit per-iteration should be at `trace!` and gated behind the env filter at `info` by default — already the v5 norm per `rules/rust.md` §function-level instrumentation. If Stage 9 measures a real hot-path bottleneck from logging, that's the motivation to revisit non-blocking; Stage 2's volume doesn't justify pre-optimization.
- **Cold-start cost of `init`**: dominated by `create_dir` + two `File::create` calls + tracing-subscriber registry construction. Well under 10 ms on a warm filesystem. Run-id allocation adds one `create_dir` per invocation (fast).
- **Log query performance**: `tail_latest_run` is a buffered reverse read of at most `lines * ~200 bytes` → tens of KB. `list_runs` is an `read_dir` + sort of typically <1000 entries. Both trivially fast.
- **Log volume**: Stage 2 emits one span per invocation. By Stage 9 we expect a few thousand events per E2E run (one per tool call, one per agent iteration, one per FSM transition). At ~500 bytes/event JSON that's ~1 MB per run — well under any filesystem's discomfort level.

### Security

- Log files under `.loopr/runs/` are excluded from git via `.git/info/exclude` (Stage 5's `loopr init` does this); events.log may contain LLM prompt contents, tool invocation args, shell command lines. Treating `.loopr/` as secrets-adjacent is the right posture even though none of v5 currently writes actual credentials there.
- **No log scrubbing in Stage 2.** If a user exports `ANTHROPIC_API_KEY=...` into a shell and then runs a tool that echoes its env, the key lands in `events.log`. Mitigation belongs with the `tools` crate (lane-level env scrubbing), not with the subscriber. Documented and deferred.
- Run-id allocation uses `create_dir`, not user-supplied strings. No path injection surface.
- `tail_latest_run` only reads files inside `<target>/.loopr/runs/`; the target is the already-canonicalized, source-guard-checked path from `lib::run`. No arbitrary-file-read surface.

### Testing Strategy

- **Unit tests** in `crates/telemetry/src/tests.rs`:
    - `RunId::allocate` — empty dir, collisions, pre-populated victims, parse.
    - `query::tail_latest_run` — empty parent, one run, multiple runs, nonexistent target.
    - `query::list_runs` — sort order, invalid-dirname skip, empty dir.
    - `compose()` helper (extracted from `init` to enable local-subscriber tests per Phase 1 note) — round-trip a `tracing::info!` event through the JSON layer to a Cursor-backed writer, parse the resulting bytes, assert expected fields. This is the core round-trip test the exit criterion demands.
- **Unit tests** in `crates/loopr/src/tests.rs`:
    - `Command::label()` — one assertion per variant; also asserts the existing `StageUnimplemented.subcommand` uses the same label.
    - `resolve_log_filter` — CLI flag wins over env, env wins over default; invalid directive errors cleanly; a per-target directive (`"loopr=debug,tools=error"`) parses and round-trips through `EnvFilter`.
- **Integration tests** in `crates/loopr/tests/smoke.rs` — as enumerated in Phase 4 (five new assertions; the collision allocator already in unit).
- **Seam tests** (per v5 working rule #2): the `telemetry::init` surface gets the round-trip test (JSON bytes in, Rust event out). The `RunId` gets the serde round-trip. The `loopr`↔`telemetry` seam (run-id allocation + init call + span emission) gets the smoke test.

### Rollout Plan

- Phases land incrementally, each as its own commit on `v5` that passes `otto ci` at workspace root.
- Bump to `v0.5.2` after Phase 4 (same repo-local override of `rules/git.md` as Stage 1's bump to `v0.5.1`).
- No coexistence — `lib::run` today has no telemetry init; Phase 3 adds it in one commit. No dual-pathed logging.
- Update `docs/roadmap.md` Stage 2 entry: (a) replace the two-doc bullet list with a single bullet pointing at this doc, (b) replace the "loopr --version emits one span" exit criterion with "any invocation that reaches `lib::run` emits the `loopr.invocation` span," (c) note the `--log-level` Stage-1 catch-up under a "Scope caveat" line.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `tracing-subscriber::fmt::format::Json` output schema changes across minor versions, breaking the round-trip test | Low | Low | Pin the version in `[workspace.dependencies]`; the round-trip test asserts named fields, not byte-identical output. When the pinned version bumps, test failure alerts us to update assertions. |
| `tracing::subscriber::set_global_default` called twice in one process (e.g. from a test that forgot to drop the previous subscriber) | Medium | Low | `AlreadyInitialized` error variant; tests use `with_default` (thread-local) instead of the global; `init()` is called exactly once from `lib::run`. |
| Per-target `.loopr/runs/` grows without bound, filling disk on a long-running dev loop | Medium | Medium | Documented. `loopr logs gc` is an earned feature; until it lands, users have `rkvr rmrf .loopr/runs/*` as a manual tool. |
| `chrono::Local::now` returns non-monotonic timestamps (DST, NTP step) making run-id ordering wrong | Low | Low | `list_runs` sorts by the parsed RunId string, which is monotonic within each wall-clock second; DST / NTP steps can produce out-of-order ids across the boundary, but the `-N` collision suffix still preserves uniqueness, and worst case two runs swap position in the `logs runs` output. Acceptable for Stage 2. |
| Console layer interferes with test harness stdout/stderr when CI runs with allocated TTYs | Low | Low | `IsTerminal` gate on stderr suppresses the console layer in piped environments, which is the normal CI configuration. Explicit smoke test covers this. |
| `thiserror` on a library crate (telemetry) plus on `loopr` creates drift on which variants live where | Low | Low | `LooprError::LogsQuery(String)` takes a stringified `QueryError` rather than a typed wrap, trading a little type fidelity for not coupling `loopr`'s error enum to `telemetry`'s. When both enums stabilize, revisit. |
| **Fork trap (Architect #1):** Stage 4's daemon-start fork would destroy non-blocking-appender worker threads, producing a daemon child writing into a dead channel | High (if not addressed) | High (silent log blackout on daemon) | Blocking `LineWriter<File>` writers in Stage 2 — no worker threads, nothing for `fork()` to annihilate. If Stage 4 measures a real need for non-blocking under daemon load, the natural place to add it is **post-fork in the daemon child's own init path**, not pre-fork in the shared `lib::run`. The client path retains blocking writers either way. |
| **Signal handling (Architect #4):** SIGINT / SIGTERM bypass `Drop`, so any partial line in `LineWriter` at the moment of interrupt is lost (at most one in-progress event, vs. 8 KiB with `BufWriter`) | Medium (any Ctrl+C) | Low (Stage 2) → Medium (Stage 4) | **Deferred to Stage 4.** Stage 2 is foreground-only and runs `StageUnimplemented` stubs — losing at most one partial line on interrupt is acceptable. Stage 4 lands `DaemonCmd::Stop` (SIGTERM issuer) and will ship the signal handler that explicitly flushes the subscriber before exit. The `Guard` type already exists in the right shape for that handler to call `.flush()` on; no structural rework needed at Stage 4 time. |
| **Pre-init ghost runs (Architect #5):** `current_dir` / `target::resolve` / `guard::check` failures happen before `telemetry::init`, so error messages reach stderr via eyre but leave no trace in any log file | Medium (any invalid `-C`) | Low | **Acknowledged, not mitigated.** Logging these failures would require initializing telemetry with a partial or defaulted target, which is worse than the current behavior (stderr-only, which any shell captures naturally). The user sees the error; the daemon isn't running yet so there's no fleet-scale observability loss. Documented here so Stage 4+ telemetry consumers don't assume every invocation produces a run dir. |
| Source-guard trips during a `loopr logs tail` query run from inside the v5 tree, preventing a developer from tailing a legitimate target's logs | Low | Low | Source-guard fires on the **effective target**, which is resolved before dispatch. If you want to tail `/tmp`'s logs, `loopr -C /tmp logs tail` works; `loopr logs tail` run from inside loopr-v5 still trips the guard and errors cleanly (message points at the sentinel). Intended behavior; no widening needed. |
| `EnvFilter::try_new("off")` or a directive that excludes all targets suppresses the `loopr.invocation` span, making `loopr --log-level off -C /tmp plan "x"` produce empty log files | Low | Low | Correct behavior, not a bug — `off` means "no events." `loopr logs tail` on a run whose files are empty reports "no output" cleanly (not an error; the run is there, just silent). Documented. |

## Open Questions

- [x] **Consolidate the two roadmap-listed docs into one?** Decided: yes, one doc. Motivation in Alternative 1. Roadmap gets amended alongside this doc landing.
- [x] **Exit criterion: `loopr --version` vs. `loopr -C /tmp plan "x"`?** Decided: the latter. Clap intercepts `--version` before dispatch, so the as-written criterion is unsatisfiable; `plan "x"` returns `StageUnimplemented` but writes logs first, which is exactly what the criterion is trying to prove.
- [x] **`.loopr/` dir bootstrap — Stage 2 creates it or requires Stage 5's `loopr init`?** Decided: Stage 2 creates `.loopr/runs/<run-id>/` on demand. `loopr init` (Stage 5) adds the rest (`.loopr/config.yml`, append to `.git/info/exclude`, install git hooks). Running Stage 2 against a `loopr init`-untouched target produces logs; `.loopr/` just happens to exist afterward.
- [x] **`--log-level` — Stage 2 scope or Stage 1 amendment?** Decided: Stage 2 adds it with a "Stage 1 gap catch-up" annotation. The Stage 1 doc gets a single-line amendment marker but is not rewritten.
- [x] **`loopr logs tail` reads `events.log` (JSON) or `loopr.log` (pretty)?** Decided: `loopr.log`. The JSON is for grep/jq; `tail` is human-facing. If we ever add `loopr logs json`, that reads the other file.
- [x] **Console layer — always on, always off, or gated?** Decided: gated on `std::io::stderr().is_terminal()`. Piped or daemonized runs skip the console; interactive terminal runs get it.
- [x] **Doc location: top-level or telemetry-scoped?** Decided: top-level (`docs/design/2026-04-19-telemetry-stage-2.md`). Root `CLAUDE.md` working rule #4: "A PR that touches two or more crates is a deliberate cross-cutting change and needs a top-level design doc." Stage 2 adds real code in both `telemetry` (subscriber, run-id, query) and `loopr` (cli flag, run() rewire, logs module), not merely workspace wiring. The cli-skeleton doc set a "meat-crate" precedent only for crate-local code that touched workspace files; this Stage 2 scope is substantively cross-cutting.
- [x] **`RunId::allocate` return type: `eyre::Report` or typed?** Decided: typed `RunIdAllocError` with `MaxRetries` and `Io { source }` variants. Retry strategies at higher layers need to tell "filesystem full" from "just collided a lot" — that's a typed-enum want, per `rules/rust.md` §Error Handling ("Libraries: thiserror with typed error enums consumers can match on").
- [x] **`init` filter type: `Level`, `LevelFilter`, or `EnvFilter`?** Decided: **`EnvFilter`** (superseding earlier `LevelFilter` decision after Architect round 1). `EnvFilter` accepts per-target directives (`"loopr=debug,tools=error"`) which preserve `tracing`'s per-crate filtering surface — the primary reason v5 picked `tracing` over the `log` crate. `LevelFilter` accepts only bare level names and would strip that capability unrecoverably. `EnvFilter::try_new("off")` covers the "silence everything" case, so we don't lose anything by the change.
- [x] **Blocking or non-blocking file writers?** Decided: **blocking `BufWriter<File>`** (superseding the original `tracing-appender::non_blocking` plan after Architect round 1). Non-blocking spawns a worker thread per file; POSIX `fork()` (Stage 4's daemon-start) destroys all threads except the caller, producing a child that writes into a dead channel. Blocking writers are fork-safe by construction. Performance is a non-issue at v5's scale. Stage 4 may revisit post-fork if daemon throughput forces it. See the fork-trap risk row.
- [x] **Reverse-scan implementation for `tail_latest_run`:** Decided: `rev_lines` crate. UTF-8-correct, maintained, iterator-style (composes with `.take(n)`). `rev_buf_reader` is lower-level (needs manual line-splitting) and a hand-rolled reverse scanner is a known trap for chunk-boundary and multi-byte UTF-8 bugs.
- [x] **Drop-guard naming vs. `rules/rust.md` "no `_` prefix":** Decided: name them `guard` and `enter` (no underscore). Drop-guards are used — the whole point is their `Drop` timing. No `_` crutch. The rule applies to "silencing real problems," which this isn't.
- [x] **Explicit `drop()` calls on `enter` / `guard` before `run` returns?** Decided (superseding an earlier "yes, explicit" position after Architect round 2): **no — rely on RAII reverse-declaration drop order.** Explicit `drop(enter); drop(guard);` flushes the LineWriters **before** the `invocation` Span handle itself is dropped, which is when `tracing` emits the span's "close" event. Flushing too early means the close event is written to a LineWriter with no subsequent flush path and is lost at process exit. RAII runs drops in reverse-declaration order: `enter` → `invocation` (close event emitted) → `guard` (flushes). The correct fix is a declaration-order comment, not explicit drops.
- [x] **`MakeWriter` impl for the shared writer?** Decided: **`SharedWriter` newtype** wrapping `Arc<Mutex<LineWriter<File>>>` + a **`SharedWriterGuard<'a>` wrapper** around `MutexGuard<'a, LineWriter<File>>` with a manual `impl io::Write`. `MakeWriter<'a>::Writer` must itself impl `io::Write`; Rust doesn't auto-forward trait impls through `Deref`, so `MutexGuard` on its own does not satisfy the bound even though it derefs to something that does. The `SharedWriterGuard` wrapper's three-line `impl io::Write` delegates to the inner guard. (Initial draft of this decision omitted the wrapper and would have failed to compile; caught by Architect round 3.)
- [x] **`BufWriter` or `LineWriter` for the per-file writers?** Decided: **`LineWriter<File>`** (superseding the initial `BufWriter<File>` plan after Architect round 2). Both amortize syscalls by buffering writes; `LineWriter` additionally flushes after every `\n`. The difference matters in Stage 4 — the daemon runs continuously, so `loopr logs tail` reads the file while writes are still happening. `BufWriter` would hide the last ~8 KiB of recent events inside the daemon's memory; `LineWriter` makes tail strictly real-time. Since every `tracing` event ends in `\n`, the per-line syscall is free at v5's volume.
- [x] **`rev_lines` error handling on torn UTF-8 at file tail?** Decided: `filter_map(Result::ok)` when consuming the `RevLines` iterator. A daemon killed mid-write (Stage 4+) can leave torn UTF-8 bytes at the tail of `loopr.log`; propagating that `Err` would crash `logs tail`. Skip-on-error lets the preceding valid lines reach the user. The cost is at most one missing line at the very end; the benefit is that the observability tool never crashes on its own subject.
- [x] **Clap parser for log-level flag:** Decided: no clap-side parser. CLI takes `Option<String>` verbatim; `EnvFilter::try_new` parses at the subscriber init site. Removes the adapter layer and the brittle dependency on clap's `value_parser!` fallthrough behavior.
- [ ] **Signal-handler integration** (Architect #4, deferred). Stage 4's `DaemonCmd::Stop` SIGTERM path, plus an always-on SIGINT handler for foreground invocations, will call an explicit `Guard::flush_now()` (or call `drop(guard)` via a `tokio::select!` on the signal future). The hook exists in Stage 2's `Guard` type; the glue lands with the daemon lifecycle design doc.
- [x] **Non-blocking writers post-fork in the daemon.** Decided: **keep the escape hatch open.** Stage 2 ships blocking `LineWriter<File>` unconditionally; `tracing-appender` stays declared in `[workspace.dependencies]` as a deliberate reservation. Stage 4 measures real daemon log throughput during the first E2E; if (and only if) a measured bottleneck justifies it, the daemon child adds a `telemetry::init_nonblocking(...)` post-fork. Client path keeps blocking forever. **Implementation note:** Cargo.toml gets a block comment above the `tracing-appender` entry explaining the "reserved for Stage 4 post-fork daemon" rationale so a future reader doesn't delete it as unused.
- [x] **Span helper macros (`span_for_stage!`, `span_for_work!`, etc.)?** Decided: **raw `#[tracing::instrument(...)]` now; macros later if the repetition-pain threshold is crossed.** The instrumentation-conventions subsection in §API Design is the permanent contract; macros would only be shorthand for that same pattern. Stage 2 ships raw; Stage 4+ watches for ≥5× copy-pasted `fields(...)` lists as the trigger. **Implementation note:** `crates/telemetry/src/lib.rs` gets a block comment at the module header documenting the "raw-instrument-now, macros-when-earned" decision so implementers of later stages know not to hand-roll shortcuts.
- [x] **When does per-Work fanout (Stage 7) actually earn its keep?** Decided (superseding the vision/roadmap/telemetry-CLAUDE "Stage 7" language): **Stage 2 ships it.** Building the `WorkFanoutLayer` alongside the other three layers while we're already in subscriber code is cheaper than a Stage 7 revisit that requires re-entering the subscriber composition. The layer runs harmlessly in Stages 2–6 (no `work_id` spans exist yet, so no fanout files get created); Stage 7 only adds the `work_id`-bearing span emitters and suddenly fanout files start appearing. See the new **Per-Work Fanout Layer** subsections in §Architecture, §Data Model, §API Design, and §Acceptance Criteria. Roadmap Stage 7 entry and vision §Observability "three-layer log strategy" both get amendments.
- [x] **`logs` subcommand self-reference — does tail/runs read its own run dir?** (Surfaced during implementation.) A `logs tail` invocation goes through `lib::run`, allocates a run-id, and initializes telemetry just like any other command — the Goals section commits to "every invocation that reaches `lib::run` emits a single `loopr.invocation` span." That creates a subtle bug: the invocation's own (still-empty) run dir is the newest, and `tail_latest_run` would return it. Decided: **`tail_latest_run` and `list_runs` accept an `Option<&RunId>` `exclude` parameter.** The logs subcommands pass their own run-id as the exclusion; other callers pass `None`. Keeps the "every invocation emits a span" invariant intact while making the user-facing query ignore the self-referential run. `LooprError` gains a `TelemetryInit(String)` variant at the same time so telemetry-setup failures don't share the `LogsQuery` channel.

## References

- [`docs/vision.md`](../../../../docs/vision.md) §Observability, §telemetry ABI, §Models and Budgets (env var naming)
- [`docs/roadmap.md`](../../../../docs/roadmap.md) §Stage 2 (to be amended alongside this doc)
- [`crates/telemetry/CLAUDE.md`](../../CLAUDE.md) §In scope, §Out of scope, §Dependencies
- [`crates/loopr/docs/design/2026-04-19-cli-skeleton.md`](../../../loopr/docs/design/2026-04-19-cli-skeleton.md) — Stage 1; `--log-level` is its missing flag
- `rules/rust.md` §Logging — flag-configured level, file target under `~/.local/share/<project>/logs/` (v5 deliberately departs: `.loopr/runs/<id>/loopr.log` per-run, not per-machine)
- `tracing-subscriber` docs: <https://docs.rs/tracing-subscriber/0.3/tracing_subscriber/>
- `tracing-appender::non_blocking`: <https://docs.rs/tracing-appender/0.2/tracing_appender/non_blocking/>
