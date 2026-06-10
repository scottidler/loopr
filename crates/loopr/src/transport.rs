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

use serde::Serialize;
use serde::de::DeserializeOwned;

pub use client::IpcClient;

use crate::config::{Config, TransportSection};
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
        let err = match IpcClient::connect(&socket, timeouts).await {
            Ok(client) => return Ok(client),
            Err(e) => e,
        };
        if Instant::now() > deadline {
            // Carry the most recent connect failure into the timeout message
            // so the operator sees WHY (ENOENT vs ECONNREFUSED vs perms),
            // not just "socket never appeared".
            return Err(LooprError::DaemonStartup(format!(
                "socket never appeared at {} (last connect error: {err})",
                socket.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

/// One-shot typed IPC call: build a current-thread runtime, connect (with
/// operator-tunable timeouts loaded from `Config`), handshake, send `method`
/// with `params`, and decode the success result as `R`. Collapses the ~7
/// hand-rolled runtime+connect+handshake+request+decode blocks that the CLI
/// verb bodies each carried.
///
/// Client timeouts honor `<target>/.loopr/config.yml`'s `transport:` section
/// (the `client-request-secs` knob, previously unreachable from the CLI).
/// A config that fails to load is **best-effort**: warn and fall back to
/// defaults rather than blocking a read verb on a malformed config — the
/// daemon's own startup is where a broken config is surfaced strictly.
pub fn ipc_call<P, R>(target: &Path, method: ipc::MethodName, params: &P) -> Result<R, LooprError>
where
    P: Serialize,
    R: DeserializeOwned,
{
    let timeouts = match Config::load(target) {
        Ok(cfg) => ClientTimeouts::from(&cfg.transport),
        Err(e) => {
            tracing::warn!(error = %e, "config load failed; using default client timeouts");
            ClientTimeouts::default()
        }
    };
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = connect_or_wait_with_timeouts(target, timeouts).await?;
        client.handshake(None).await?;
        let params_value =
            serde_json::to_value(params).map_err(|e| LooprError::ClientIo(format!("serialize {method} params: {e}")))?;
        let (resp, _events) = client.request(method, params_value).await?;
        if let Some(err) = resp.error {
            return Err(LooprError::Rpc(err));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::ClientIo(format!("{method} response missing result")))?;
        serde_json::from_value(result_value).map_err(|e| LooprError::ClientIo(format!("decode {method}: {e}")))
    })
}
