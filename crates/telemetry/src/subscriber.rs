use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, LineWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use thiserror::Error;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt};

use crate::fanout::WorkFanoutLayer;
use crate::process::ProcessId;
use crate::session::SessionId;
use crate::session_fanout::SessionFanoutLayer;
use crate::xdg;

/// Newtype wrapper that makes `Arc<Mutex<LineWriter<File>>>` usable as a
/// `MakeWriter` for `tracing-subscriber::fmt::Layer`. Required because:
///   - `tracing_subscriber::fmt::MakeWriter` has no blanket impl for
///     `Arc<Mutex<W>>`.
///   - `MutexGuard<'a, W>` does NOT auto-forward the `io::Write` impl of `W`;
///     Rust doesn't forward trait impls through `Deref`. So we also need a
///     `SharedWriterGuard` wrapper whose own `io::Write` impl delegates to
///     the inner guard's `DerefMut` target.
///
/// Cloneable so the subscriber can hold one copy and the `Guard` another.
#[derive(Clone)]
pub struct SharedWriter(pub(crate) Arc<Mutex<LineWriter<File>>>);

impl SharedWriter {
    pub(crate) fn new(file: File) -> Self {
        SharedWriter(Arc::new(Mutex::new(LineWriter::new(file))))
    }
}

/// Holds a locked `MutexGuard` and forwards `io::Write` to the inner
/// `LineWriter<File>`. Lives for the duration of one `make_writer()` call
/// (i.e. one `tracing` event emission).
pub struct SharedWriterGuard<'a>(MutexGuard<'a, LineWriter<File>>);

impl Write for SharedWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<'a> MakeWriter<'a> for SharedWriter {
    type Writer = SharedWriterGuard<'a>;
    fn make_writer(&'a self) -> Self::Writer {
        SharedWriterGuard(self.0.lock().expect("log writer mutex poisoned"))
    }
}

/// Drop-guard that flushes the line-buffered file writers on graceful exit.
///
/// **Why `LineWriter`, not `BufWriter`**: both amortize syscalls by buffering,
/// but `LineWriter` guarantees a flush after every `\n`. This matters for
/// Stage 4 - the daemon is long-lived, so `loopr logs tail` reads the file
/// while the daemon is still writing. `BufWriter` would let up to ~8 KiB of
/// recent events sit in daemon memory invisible to `tail`; `LineWriter`
/// flushes each `tracing` event immediately (every emit ends in `\n`),
/// keeping tailing strictly real-time.
///
/// **Why a Guard at all**: belt-and-suspenders for the graceful-exit path.
/// `LineWriter` flushes on newline and on its own Drop, but the LineWriter is
/// held by the global subscriber which never gets dropped (globals leak at
/// process exit). The explicit `Guard::drop` reaches through the Arc and
/// flushes the final partial line (if any) before the process exits.
#[must_use = "telemetry::Guard must be held for the lifetime of the invocation; dropping early truncates logs"]
pub struct Guard {
    json_writer: SharedWriter,
    pretty_writer: SharedWriter,
    fanout_cache: Arc<dashmap::DashMap<String, SharedWriter>>,
    session_fanout_cache: Arc<Mutex<lru::LruCache<String, SharedWriter>>>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Ok(mut w) = self.json_writer.0.lock() {
            let _ = w.flush();
        }
        if let Ok(mut w) = self.pretty_writer.0.lock() {
            let _ = w.flush();
        }
        for entry in self.fanout_cache.iter() {
            if let Ok(mut w) = entry.value().0.lock() {
                let _ = w.flush();
            }
        }
        if let Ok(cache) = self.session_fanout_cache.lock() {
            for (_id, writer) in cache.iter() {
                if let Ok(mut w) = writer.0.lock() {
                    let _ = w.flush();
                }
            }
        }
    }
}

/// Initialize the global tracing subscriber for this process.
///
/// Creates the per-process run dir under XDG at
/// `$XDG_DATA_HOME/loopr/sessions/<session>/targets/<target_slug>/runs/<process>/`,
/// opens `events.log` and `loopr.log` there, and composes:
///   1. JSON layer    -> events.log (filtered by `filter`)
///   2. Pretty layer  -> loopr.log  (filtered by `filter`)
///   3. Console layer -> stderr     (INFO and above, gated on IsTerminal)
///   4. Fanout layer  -> work/<work_id>.log per Work-scoped event
///
/// Writers are blocking, not `tracing-appender::non_blocking`, to dodge the
/// fork trap (Stage 4's daemon child would inherit dead channels from a
/// non-blocking appender initialized pre-fork).
///
/// `_target` is retained as a parameter for call-site symmetry with earlier
/// versions and for the daemon's legacy-runs-dir warning (Phase 9); the
/// subscriber writes exclusively to XDG and does not touch the target tree.
///
/// Directive is an `EnvFilter`-parseable string (e.g. `"info"`,
/// `"loopr=debug,tools=error"`, `"off"`). `init` validates it once at the top
/// before touching the filesystem; an invalid directive surfaces as
/// `InvalidFilter` without leaving a ghost run directory behind. Each layer
/// then parses its own fresh `EnvFilter` from the same string (cheaper than
/// and no less correct than `EnvFilter::Clone`, which does not exist).
pub fn init(
    _target: &Path,
    session_id: &SessionId,
    target_slug: &str,
    process_id: &ProcessId,
    directive: &str,
) -> Result<Guard, TelemetryInitError> {
    // Validate before any I/O: a bad directive must not leave a stale run dir.
    EnvFilter::try_new(directive).map_err(|e| TelemetryInitError::InvalidFilter {
        directive: directive.to_string(),
        reason: e.to_string(),
    })?;
    let console_directive = floor_at_info(directive);
    // Validate the floored directive too: it is a straight transformation of
    // `directive` and should always re-parse, but a belt-and-suspenders check
    // keeps any future `floor_at_info` regression from shipping silently.
    EnvFilter::try_new(&console_directive).map_err(|e| TelemetryInitError::InvalidFilter {
        directive: console_directive.clone(),
        reason: e.to_string(),
    })?;

    let run_dir =
        xdg::session_run_dir(session_id, target_slug, process_id).map_err(|e| TelemetryInitError::DirCreate {
            path: std::path::PathBuf::from(format!("xdg session_run_dir: {e}")),
            source: io::Error::other(e.to_string()),
        })?;

    let xdg_root = xdg::xdg_root().map_err(|e| TelemetryInitError::DirCreate {
        path: std::path::PathBuf::from(format!("xdg_root: {e}")),
        source: io::Error::other(e.to_string()),
    })?;

    let json_path = run_dir.join("events.log");
    let pretty_path = run_dir.join("loopr.log");
    let json_file = open_append(&json_path)?;
    let pretty_file = open_append(&pretty_path)?;

    let json_writer = SharedWriter::new(json_file);
    let pretty_writer = SharedWriter::new(pretty_file);

    let fanout = WorkFanoutLayer::new(&run_dir);
    let fanout_cache = fanout.cache_handle();

    let session_fanout = SessionFanoutLayer::new(xdg_root, target_slug.to_string());
    let session_fanout_cache = session_fanout.cache_handle();

    compose(
        directive,
        &console_directive,
        json_writer.clone(),
        pretty_writer.clone(),
        fanout,
        session_fanout,
        io::stderr().is_terminal(),
    )?;

    Ok(Guard {
        json_writer,
        pretty_writer,
        fanout_cache,
        session_fanout_cache,
    })
}

/// Test-only entry point: build a thread-local subscriber whose layer
/// composition mirrors production `init`'s, writing `events.log` /
/// `loopr.log` under `run_dir`. Returns a guard that uninstalls the
/// subscriber and flushes the writers when dropped.
///
/// Sharing the layer builder (`compose_subscriber`) with production is
/// deliberate — it is the mechanism that lets the contract test in
/// `crates/telemetry/tests/events_log_contract.rs` catch regressions in
/// production layer shape.
///
/// # Sync-only constraint
///
/// `set_default` is **thread-local**: events emitted on a different thread
/// (notably tokio worker threads spawned via `tokio::spawn`) will not
/// reach the subscriber installed here. Phase-1 smoke tests are sync;
/// future phases that exercise async code (e.g. decomposer) will need a
/// parallel async-aware harness or `tracing::instrument`-on-future
/// adaptation. This helper deliberately does not try to bridge that gap.
pub fn init_for_test(run_dir: &Path, directive: &str) -> Result<TestSubscriberGuard, TelemetryInitError> {
    // Install the process-global always-interested default first, so the
    // per-callsite interest cache can never resolve to `never` on a
    // subscriber-less sibling thread and empty a later capture buffer. See
    // `crate::testing` for the full mechanism. Idempotent and cheap.
    crate::testing::ensure_global_interested_default();

    EnvFilter::try_new(directive).map_err(|e| TelemetryInitError::InvalidFilter {
        directive: directive.to_string(),
        reason: e.to_string(),
    })?;
    let console_directive = floor_at_info(directive);
    EnvFilter::try_new(&console_directive).map_err(|e| TelemetryInitError::InvalidFilter {
        directive: console_directive.clone(),
        reason: e.to_string(),
    })?;

    std::fs::create_dir_all(run_dir).map_err(|source| TelemetryInitError::DirCreate {
        path: run_dir.to_path_buf(),
        source,
    })?;

    let json_path = run_dir.join("events.log");
    let pretty_path = run_dir.join("loopr.log");
    let json_file = open_append(&json_path)?;
    let pretty_file = open_append(&pretty_path)?;

    let json_writer = SharedWriter::new(json_file);
    let pretty_writer = SharedWriter::new(pretty_file);

    let fanout = WorkFanoutLayer::new(run_dir);
    let fanout_cache = fanout.cache_handle();

    // Per-session fanout writes under `run_dir/sessions/...` so it can never
    // escape the tempdir. The smoke scenarios don't exercise the session
    // fanout — its presence here keeps the test's layer composition
    // identical to production.
    let session_fanout = SessionFanoutLayer::new(run_dir.to_path_buf(), "test-target".to_string());
    let session_fanout_cache = session_fanout.cache_handle();

    // Tests never want stderr noise; pass `stderr_is_tty=false` so the
    // console layer is skipped.
    let subscriber = compose_subscriber(
        directive,
        &console_directive,
        json_writer.clone(),
        pretty_writer.clone(),
        fanout,
        session_fanout,
        false,
    );
    let default_guard = tracing::subscriber::set_default(subscriber);

    Ok(TestSubscriberGuard {
        // Drop order matters: `_default` first (uninstall thread-local
        // subscriber so no further events route through these writers),
        // then `_writers` (Guard's Drop flushes any line-buffered partials
        // before the test reads `events.log`). Field declaration order is
        // drop order in Rust.
        _default: default_guard,
        _writers: Guard {
            json_writer,
            pretty_writer,
            fanout_cache,
            session_fanout_cache,
        },
    })
}

/// Guard returned by [`init_for_test`]. Holds the thread-local default
/// subscriber installation alive plus the writer-flush guard.
#[must_use = "TestSubscriberGuard must be held until the scenario completes; dropping early uninstalls the subscriber"]
pub struct TestSubscriberGuard {
    _default: tracing::subscriber::DefaultGuard,
    _writers: Guard,
}

fn open_append(path: &Path) -> Result<File, TelemetryInitError> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|source| TelemetryInitError::FileOpen {
            path: path.to_path_buf(),
            source,
        })
}

/// Build the layered subscriber. Separated from `init` so tests can install
/// the same shape via `set_default` (thread-local) instead of `try_init`
/// (process-global). Sharing this builder is the keystone of the contract
/// test from `docs/design/2026-05-09-comprehensive-telemetry.md`: a
/// regression in production layer composition is caught by the contract
/// test only because both paths route through here.
///
/// `directive` is the user's raw filter (used by json, pretty, fanout).
/// `console_directive` is the same directive with an INFO floor applied — all
/// four layers therefore carry the same `EnvFilter` type, which keeps a
/// future runtime-reload (e.g. Stage 4's hypothetical `loopr logs tail
/// --level debug`) straightforward.
fn compose_subscriber(
    directive: &str,
    console_directive: &str,
    json_writer: SharedWriter,
    pretty_writer: SharedWriter,
    fanout: WorkFanoutLayer,
    session_fanout: SessionFanoutLayer,
    stderr_is_tty: bool,
) -> impl tracing::Subscriber + Send + Sync + 'static {
    // `directive` has been validated by `init` (or constructed directly in
    // tests); every `expect` below is an invariant, not a runtime gamble.
    let json_layer = fmt::layer()
        .json()
        .with_writer(json_writer)
        .with_filter(parse_validated(directive));

    let pretty_layer = fmt::layer()
        .with_writer(pretty_writer)
        .with_ansi(false)
        .with_filter(parse_validated(directive));

    // Console filter: `directive` with every trace/debug clause floored to
    // info. An event reaches stderr only if the floored filter admits it, so:
    //   --log-level warn   -> stderr shows warn+ (user filter wins)
    //   --log-level debug  -> stderr shows info+ (floor prevents spam)
    //   --log-level off    -> stderr shows nothing
    // Files (events.log + loopr.log) still honor the full user directive.
    let console_layer = if stderr_is_tty {
        Some(
            fmt::layer()
                .with_writer(io::stderr)
                .with_filter(parse_validated(console_directive)),
        )
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(json_layer)
        .with(pretty_layer)
        .with(console_layer)
        .with(fanout.with_filter(parse_validated(directive)))
        .with(session_fanout.with_filter(parse_validated(directive)))
}

/// Install the layered subscriber as this process's global subscriber.
/// Production-only path; tests use `init_for_test` which routes through
/// the same `compose_subscriber` builder but installs thread-locally.
fn compose(
    directive: &str,
    console_directive: &str,
    json_writer: SharedWriter,
    pretty_writer: SharedWriter,
    fanout: WorkFanoutLayer,
    session_fanout: SessionFanoutLayer,
    stderr_is_tty: bool,
) -> Result<(), TelemetryInitError> {
    compose_subscriber(
        directive,
        console_directive,
        json_writer,
        pretty_writer,
        fanout,
        session_fanout,
        stderr_is_tty,
    )
    .try_init()
    .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
    Ok(())
}

/// Parse a directive that has already been validated by the caller. Any
/// failure here is a bug (the caller's invariant was wrong), not a user
/// input problem, so panicking is correct — and better than silently
/// falling back to `info` and masking the regression.
fn parse_validated(directive: &str) -> EnvFilter {
    EnvFilter::try_new(directive).expect("directive validated by caller")
}

/// Apply an INFO floor to every clause of an EnvFilter directive. Clauses
/// whose level is more permissive than INFO (`trace`, `debug`) are clamped
/// to `info`; `info`, `warn`, `error`, and `off` pass through unchanged.
///
/// Works on the common subset of EnvFilter syntax — bare levels, `target=level`,
/// and comma-separated combinations. Exotic forms (span/field filters inside
/// `[...]`) are left untouched; those directives are rare and tend to mean
/// "I know what I'm doing" anyway.
pub(crate) fn floor_at_info(directive: &str) -> String {
    directive
        .split(',')
        .map(|clause| {
            let trimmed = clause.trim();
            if trimmed.is_empty() {
                return trimmed.to_string();
            }
            // Field / span / regex filters contain `[` or `{`; keep them
            // verbatim rather than mangling structure we don't parse.
            if trimmed.contains('[') || trimmed.contains('{') {
                return trimmed.to_string();
            }
            match trimmed.rsplit_once('=') {
                Some((target, level)) => format!("{}={}", target, floor_level(level.trim())),
                None => floor_level(trimmed).to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn floor_level(level: &str) -> &str {
    match level.trim().to_ascii_lowercase().as_str() {
        "trace" | "debug" => "info",
        _ => level,
    }
}

#[derive(Error, Debug)]
pub enum TelemetryInitError {
    #[error("telemetry::init called twice in the same process")]
    AlreadyInitialized,
    #[error("failed to create runs dir {path}: {source}", path = .path.display())]
    DirCreate { path: PathBuf, source: io::Error },
    #[error("failed to open log file {path}: {source}", path = .path.display())]
    FileOpen { path: PathBuf, source: io::Error },
    #[error("invalid log filter `{directive}`: {reason}")]
    InvalidFilter { directive: String, reason: String },
}
