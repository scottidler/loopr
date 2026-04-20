//! `DaemonContext`: shared state for the daemon run.
//!
//! Held in an `Arc` by the accept loop, each connection-handler task, and
//! the signal-watcher task. Values are set once at startup and read-only
//! thereafter; the only mutable cell is `shutting_down`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::{Notify, broadcast};

use ipc::DaemonEvent;
use telemetry::RunId;

/// Capacity of the daemon's event broadcast channel. Stage 4 never sends
/// on it; the capacity is future-proofing for Stage 7+. v4 value.
pub const EVENTS_CAPACITY: usize = 64;

pub struct DaemonContext {
    pub target: PathBuf,
    pub run_id: RunId,
    pub started_at: chrono::DateTime<chrono::Local>,
    pub pid: u32,
    /// Broadcast bus for `DaemonEvent`s. Stage 4 defines the channel but
    /// never fires an event. Stage 5+ fires on record transitions.
    pub events: broadcast::Sender<DaemonEvent>,
    /// Set to `true` by the signal-watcher task or by an in-process
    /// shutdown request. `accept_loop` and every `handle_client` read it
    /// to decide whether to exit.
    pub shutting_down: Arc<AtomicBool>,
    /// Async-friendly wakeup. A single `tokio::signal`-driven task awaits
    /// SIGTERM/SIGINT; on signal it sets `shutting_down = true` and calls
    /// `shutdown_notify.notify_waiters()`, which wakes every consumer
    /// that called `shutdown_notify.notified().await`. This avoids
    /// polling.
    ///
    /// NOTE: the signal-watcher runs as a `tokio::spawn` task, not as a
    /// POSIX signal handler: `tokio::signal::unix::signal(SIGTERM)?.recv()`
    /// delivers the signal as an async value. No `async-signal-safe`
    /// constraints apply because we never touch tokio from a true signal
    /// handler context.
    pub shutdown_notify: Arc<Notify>,
}

impl DaemonContext {
    /// Construct a new context. All fields are set once at daemon startup;
    /// nothing mutable is exposed except the `shutting_down` atomic.
    pub fn new(target: PathBuf, run_id: RunId, pid: u32) -> Self {
        let (events, _) = broadcast::channel(EVENTS_CAPACITY);
        Self {
            target,
            run_id,
            started_at: chrono::Local::now(),
            pid,
            events,
            shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
        }
    }
}
