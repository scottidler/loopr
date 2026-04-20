//! IPC transport: `tokio`-based Unix socket accept loop on the daemon side
//! and a short-lived `IpcClient` on the client side. Consumes `ipc` crate
//! types via `LinesCodec`. Does not know about fork, pid files, or process
//! lifetime -- that's `daemon`'s job.
//!
//! Submodules: `server` (accept loop + per-connection handler), `client`
//! (`IpcClient` + handshake), `handler` (exhaustive `ipc::Method` dispatch).

pub(crate) mod client;
pub(crate) mod handler;
pub(crate) mod server;

/// Polling cadence used by `connect_or_wait` while waiting for a freshly-
/// forked daemon to bind its socket. v4 value; matches `POLL_INTERVAL_MS`
/// in the Stage 4 design doc.
pub const POLL_INTERVAL_MS: u64 = 100;

/// Ceiling on how long `connect_or_wait` will poll before giving up and
/// returning `LooprError::DaemonStartup`. v4 value; matches the design
/// doc's `START_TIMEOUT_SECS`.
pub const START_TIMEOUT_SECS: u64 = 3;
