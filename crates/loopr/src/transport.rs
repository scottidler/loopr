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

use std::path::Path;
use std::time::{Duration, Instant};

pub use client::IpcClient;

use crate::daemon::sentinel;
use crate::error::LooprError;

/// Polling cadence used by `connect_or_wait` while waiting for a freshly-
/// forked daemon to bind its socket. v4 value; matches `POLL_INTERVAL_MS`
/// in the Stage 4 design doc.
pub const POLL_INTERVAL_MS: u64 = 100;

/// Ceiling on how long `connect_or_wait` will poll before giving up and
/// returning `LooprError::DaemonStartup`. v4 value; matches the design
/// doc's `START_TIMEOUT_SECS`.
pub const START_TIMEOUT_SECS: u64 = 3;

/// Connect to the daemon at `<target>/.loopr/socket`. Polls for the
/// socket up to `START_TIMEOUT_SECS` at `POLL_INTERVAL_MS` cadence; by
/// the time any async code runs, `lib::run`'s pre-telemetry hoist has
/// already decided whether to fork a daemon, so this function only
/// needs to wait for the grandchild's `bind`. It never forks: that
/// authority lives in `daemon::ensure_daemon[_if_needed]`.
#[tracing::instrument(name = "client.connect_or_wait", level = "debug", skip_all, fields(target = %target.display()), err)]
pub async fn connect_or_wait(target: &Path) -> Result<IpcClient, LooprError> {
    let socket = sentinel::socket_path(target);
    let deadline = Instant::now() + Duration::from_secs(START_TIMEOUT_SECS);
    loop {
        if let Ok(client) = IpcClient::connect(&socket).await {
            return Ok(client);
        }
        if Instant::now() > deadline {
            return Err(LooprError::DaemonStartup(format!(
                "socket never appeared at {}",
                socket.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}
