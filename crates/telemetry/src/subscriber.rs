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
use crate::runid::RunId;

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
    }
}

/// Initialize the global tracing subscriber for this process.
///
/// Creates `<target>/.loopr/runs/<run_id>/` if it does not exist, opens
/// `events.log` and `loopr.log` there as blocking `LineWriter<File>` handles
/// wrapped in `Arc<Mutex<_>>` via the `SharedWriter` newtype, and composes:
///   1. JSON layer    -> events.log (filtered by `filter`)
///   2. Pretty layer  -> loopr.log  (filtered by `filter`)
///   3. Console layer -> stderr     (INFO and above, gated on IsTerminal)
///   4. Fanout layer  -> work/<work_id>.log per Work-scoped event
///
/// Writers are blocking, not `tracing-appender::non_blocking`, to dodge the
/// fork trap (Stage 4's daemon child would inherit dead channels from a
/// non-blocking appender initialized pre-fork).
///
/// Filter is `EnvFilter` to preserve `tracing`'s per-target directive surface.
pub fn init(target: &Path, run_id: &RunId, filter: EnvFilter) -> Result<Guard, TelemetryInitError> {
    let run_dir = target.join(".loopr").join("runs").join(run_id.as_str());
    std::fs::create_dir_all(&run_dir).map_err(|source| TelemetryInitError::DirCreate {
        path: run_dir.clone(),
        source,
    })?;

    let json_path = run_dir.join("events.log");
    let pretty_path = run_dir.join("loopr.log");
    let json_file = open_append(&json_path)?;
    let pretty_file = open_append(&pretty_path)?;

    let json_writer = SharedWriter::new(json_file);
    let pretty_writer = SharedWriter::new(pretty_file);

    let fanout = WorkFanoutLayer::new(&run_dir);
    let fanout_cache = fanout.cache_handle();

    compose(
        filter,
        json_writer.clone(),
        pretty_writer.clone(),
        fanout,
        io::stderr().is_terminal(),
    )?;

    Ok(Guard {
        json_writer,
        pretty_writer,
        fanout_cache,
    })
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

/// Build and install the subscriber. Separated from `init` so tests can
/// exercise subscriber composition with local `SharedWriter` instances via
/// `with_default`, avoiding the global once-per-process constraint.
fn compose(
    filter: EnvFilter,
    json_writer: SharedWriter,
    pretty_writer: SharedWriter,
    fanout: WorkFanoutLayer,
    stderr_is_tty: bool,
) -> Result<(), TelemetryInitError> {
    let json_layer = fmt::layer()
        .json()
        .with_writer(json_writer)
        .with_filter(filter_clone(&filter));

    let pretty_layer = fmt::layer()
        .with_writer(pretty_writer)
        .with_ansi(false)
        .with_filter(filter_clone(&filter));

    let console_layer = if stderr_is_tty {
        Some(fmt::layer().with_writer(io::stderr).with_filter(EnvFilter::new("info")))
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(json_layer)
        .with(pretty_layer)
        .with(console_layer)
        .with(fanout.with_filter(filter))
        .try_init()
        .map_err(|_| TelemetryInitError::AlreadyInitialized)?;
    Ok(())
}

/// `EnvFilter` doesn't impl `Clone`. Re-parse its string representation.
/// The serialized form is lossless for filter directives we care about.
fn filter_clone(f: &EnvFilter) -> EnvFilter {
    EnvFilter::try_new(f.to_string()).unwrap_or_else(|_| EnvFilter::new("info"))
}

#[derive(Error, Debug)]
pub enum TelemetryInitError {
    #[error("telemetry::init called twice in the same process")]
    AlreadyInitialized,
    #[error("failed to create runs dir {path}: {source}", path = .path.display())]
    DirCreate { path: PathBuf, source: io::Error },
    #[error("failed to open log file {path}: {source}", path = .path.display())]
    FileOpen { path: PathBuf, source: io::Error },
}
