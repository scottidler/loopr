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

use crate::config::TransportSection;
use crate::daemon::sentinel;
use crate::error::LooprError;

/// Client-side IPC timeouts. Production builds these from
/// `TransportSection` via `From`, so the operator-facing knob in
/// `.loopr/config.yml` is the single source of truth for the numbers.
/// `Default` is provided as a convenience that routes through
/// `TransportSection::default()` — same numbers, no duplicate constants.
#[derive(Debug, Clone, Copy)]
pub struct ClientTimeouts {
    /// Wall-clock cap on `request_impl` (send + response loop).
    pub request: Duration,
}

impl From<&TransportSection> for ClientTimeouts {
    fn from(t: &TransportSection) -> Self {
        Self {
            request: Duration::from_secs(t.client_request_secs),
        }
    }
}

impl Default for ClientTimeouts {
    fn default() -> Self {
        Self::from(&TransportSection::default())
    }
}

/// Server-side IPC timeouts. Production builds these from
/// `TransportSection` via `From`. Held on `DaemonContext` so every
/// `handle_client` task reads the same budgets. `Default` routes through
/// `TransportSection::default()` so the numbers stay in one place.
#[derive(Debug, Clone, Copy)]
pub struct ServerTimeouts {
    /// Wall-clock cap on read silence inside `handle_client`. Reset only
    /// on real client traffic from `framed.next()`; broadcasts do not reset.
    pub idle: Duration,
    /// Wall-clock cap on each `framed.send(...).await` inside
    /// `handle_client` (response-write and event-broadcast-write).
    pub write: Duration,
}

impl From<&TransportSection> for ServerTimeouts {
    fn from(t: &TransportSection) -> Self {
        Self {
            idle: Duration::from_secs(t.server_idle_secs),
            write: Duration::from_secs(t.server_write_secs),
        }
    }
}

impl Default for ServerTimeouts {
    fn default() -> Self {
        Self::from(&TransportSection::default())
    }
}

/// Polling cadence used by `connect_or_wait` while waiting for a freshly-
/// forked daemon to bind its socket. v4 value; matches `POLL_INTERVAL_MS`
/// in the Stage 4 design doc.
pub const POLL_INTERVAL_MS: u64 = 100;

/// Ceiling on how long `connect_or_wait` will poll before giving up and
/// returning `LooprError::DaemonStartup`. v4 value; matches the design
/// doc's `START_TIMEOUT_SECS`.
pub const START_TIMEOUT_SECS: u64 = 3;

/// Connect to the daemon at `<target>/.loopr/socket` using the
/// no-config-file defaults from `TransportSection::default()`. Short-
/// lived CLI invocations call this; commands that want operator-tunable
/// budgets call `connect_or_wait_with_timeouts` after loading `Config`.
pub async fn connect_or_wait(target: &Path) -> Result<IpcClient, LooprError> {
    connect_or_wait_with_timeouts(target, ClientTimeouts::from(&TransportSection::default())).await
}

/// Connect with explicit `ClientTimeouts`. Polls for the socket up to
/// `START_TIMEOUT_SECS` at `POLL_INTERVAL_MS` cadence; by the time any
/// async code runs, `lib::run`'s pre-telemetry hoist has already
/// decided whether to fork a daemon, so this function only needs to
/// wait for the grandchild's `bind`. It never forks: that authority
/// lives in `daemon::ensure_daemon[_if_needed]`.
#[tracing::instrument(name = "client.connect_or_wait", level = "debug", skip_all, fields(target = %target.display()), err)]
pub async fn connect_or_wait_with_timeouts(target: &Path, timeouts: ClientTimeouts) -> Result<IpcClient, LooprError> {
    let socket = sentinel::socket_path(target);
    let deadline = Instant::now() + Duration::from_secs(START_TIMEOUT_SECS);
    loop {
        if let Ok(client) = IpcClient::connect(&socket, timeouts).await {
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
