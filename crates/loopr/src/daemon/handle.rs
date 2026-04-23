//! `DaemonHandle`: caller-facing shutdown + socket-path accessor.
//!
//! Clones the `DaemonContext`'s shutdown atomics (Arc<AtomicBool> +
//! Arc<Notify>) and computes the socket path via `sentinel::socket_path`.
//! Does NOT hold an `Arc<DaemonContext>` clone — that would strand the
//! context past `Arc::try_unwrap` at shutdown. Tests spawn `serve_core`
//! into a `tokio::task` and drive shutdown through this handle.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::Notify;

use llm::LlmClient;

use crate::daemon::DaemonContext;
use crate::daemon::sentinel;

#[derive(Clone)]
pub struct DaemonHandle {
    shutting_down: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    socket_path: PathBuf,
}

impl DaemonHandle {
    /// Clone the shutdown atomics and derive the socket path so the
    /// handle's view of the socket matches where `serve_core` binds.
    pub fn from_context<L: LlmClient + Send + Sync + 'static>(ctx: &Arc<DaemonContext<L>>) -> Self {
        Self {
            shutting_down: ctx.shutting_down.clone(),
            shutdown_notify: ctx.shutdown_notify.clone(),
            socket_path: sentinel::socket_path(&ctx.target),
        }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Request shutdown. Non-blocking: flips the atomic flag, then wakes
    /// every waiter of `shutdown_notify`. The daemon's accept loop,
    /// integrator backoff sleep, and reviewer wait sites all select on
    /// `shutdown_notify.notified()`, so this unblocks them.
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::Relaxed);
        self.shutdown_notify.notify_waiters();
    }
}
