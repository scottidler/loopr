# Design Document: Daemon Fork + IPC Transport (Stage 4)

**Author:** Scott A. Idler
**Date:** 2026-04-19
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Stand up the `loopr` binary's daemon lifecycle and IPC transport: a double-fork detachment that produces a background daemon bound to `.loopr/socket`, a PID-and-version lockfile at `.loopr/daemon.pid`, a `tokio`-based accept loop that consumes `ipc` crate types via `LinesCodec`, a client transport that connects (or auto-forks the daemon if absent) and performs the mandatory `system.handshake`, and exhaustive `ipc::Method` dispatch for the two Stage-3 methods. Stage 4 of `docs/roadmap.md`. Consolidates the roadmap's originally-listed two docs (`daemon-lifecycle.md` + `ipc-transport.md`) into one because lifecycle and transport cannot be reviewed independently — the socket's lifetime IS the daemon's lifetime, and the PID file guards the socket. Same precedent as Stage 2's telemetry consolidation.

## Problem Statement

### Background

Stage 1 (`2026-04-19-cli-skeleton.md`) gave loopr a typed CLI shell. Stage 2 (`2026-04-19-telemetry-stage-2.md`) gave it structured per-run logging. Stage 3 (`2026-04-19-protocol.md`) defined the wire protocol — envelope types, `Method` enum, `RpcError` closed enum, `encode_line`/`decode_line`, the `MAX_LINE_BYTES` and `PROTOCOL_VERSION` consts. Stage 3 is I/O-free by design (`crates/ipc/CLAUDE.md`: "no `tokio`, no sockets"); it ships the types and the round-trip proofs, nothing that actually reads or writes a byte.

Stage 4 is where those types go on the wire. `docs/vision.md` §Target Repo Layout pins the filesystem surface: `.loopr/socket`, `.loopr/daemon.pid`, and (implicitly, from v3/v4 precedent) `.loopr/daemon.version` alongside the pid file so a rebuilt daemon can detect version drift and silently restart. §loopr says the binary "parses CLI, forks-to-daemon or connects-as-client" and places the socket transport in this crate, not in `ipc`. The loopr `CLAUDE.md` makes the transport scope explicit: "Unix socket bind/accept, `tokio::net::UnixListener`/`UnixStream`, `LinesCodec` hookup with the 1 MiB max line from v3/v4, client connect lifecycle, stale-socket detection, PID lockfile."

v4's `src/daemon.rs`, `src/ipc/server.rs`, `src/ipc/client.rs`, and `src/ipc/codec.rs` are the reference implementation. They shipped and worked. v5 inherits the double-fork lifecycle, the `Framed<UnixStream, LinesCodec>` transport, the request-response + broadcast-event connection model, and the auto-start-on-connect client ergonomic that v4 users grew to rely on. What changes in v5: the wire types come from a separate `ipc` crate (not an inline `ipc/` module); dispatch is exhaustive over `ipc::Method` (not string-matched on `req.method`); the handshake is mandatory on every connection (per the Stage 3 "Handshake ordering" contract); and malformed requests on the daemon's inbound stream close the connection instead of synthesizing a fake-id error response (per the Stage 3 "Unparseable input" contract).

### Problem

- No socket ever binds. Stage 3's `encode_line` / `decode_line` have no consumer. `loopr daemon start` returns `StageUnimplemented { stage: 4 }`.
- No daemon process ever runs in the background. `loopr daemon start --foreground` has nothing to run in the foreground either.
- No client ever connects. `loopr daemon status` (which is the cleanest smoke-test for a round-trip) returns `StageUnimplemented { stage: 4 }`; there is no code path that opens a `UnixStream` to `.loopr/socket`.
- The `tokio` runtime is not a `loopr` dependency. Neither is `tokio_util` (for `LinesCodec`) nor `libc` (for `fork`/`setsid`). Stage 4's first mechanical act is to wire these via `cargo add`.
- There is no handler code that matches exhaustively on `ipc::Method`. The "add a variant to `Method`, get a compile error in the handler" guarantee that Stage 3 designed for has no handler to fire against.
- Stage 3's "Handshake ordering" contract ("daemon MUST require `system.handshake` as the first method on every new connection") is a contract in prose. Stage 4 is where it becomes a state machine in `loopr::transport`.
- Stage 3's "Unparseable input on the daemon's inbound stream" contract ("the transport layer MUST NOT synthesize a fake `id` to satisfy `DaemonResponse::id: u64` — a synthesized id can collide with a legitimate client request id and corrupt correlation state") is a contract in prose. Stage 4 is where `decode_request_line` failures close the connection.
- The Stage 4 exit criterion from `docs/roadmap.md` — "two terminals: one runs the daemon, the other runs `loopr plan \"x\"`; the second terminal gets an ACK and the daemon's log shows the request arriving with the right `run_id`" — is not satisfiable today because neither end of the wire exists.

### Goals

- `loopr daemon start` forks to a background daemon. On success: the caller's shell returns immediately; `.loopr/socket` exists; `.loopr/daemon.pid` contains the daemon's PID; `.loopr/daemon.version` contains the exact `GIT_DESCRIBE` string of the binary that forked; `.loopr/daemon.run-id` contains the daemon's allocated run-id (so tests and clients can locate the daemon's log directory without a round-trip); the daemon holds its own `telemetry::Guard` and writes to `<target>/.loopr/runs/<daemon-run-id>/`.
- **Telemetry init is hoisted above every fork seam, parent-side, in `lib::run`.** `tracing::subscriber::set_global_default` is call-once per process memory; `fork()` copies the "already set" bit into the grandchild, so a second init in the grandchild fails. The fix: `lib::run` handles fork-triggering commands BEFORE its telemetry-init block:
    - `Command::Daemon { cmd: DaemonCmd::Start { foreground: false } }`: call `daemon::ensure_daemon(&effective)` directly; the parent prints "daemon started" and exits (never inits telemetry). The grandchild inits its own inside `daemon_main`.
    - Commands that require a live daemon (`Plan`, `Decompose`, `Execute`, `Integrate`, `Daemon { cmd: Status }`): call `daemon::ensure_daemon_if_needed(&effective)` BEFORE the telemetry init. If the daemon is already running + version-matching, this returns immediately (no fork, parent continues normally). If the daemon needs to be forked, the fork happens here, THEN the parent inits its client-side telemetry, THEN dispatch proceeds. Either way the grandchild has already exited `lib::run`'s control flow (via `process::exit` after `daemon_main`) and never reaches the parent's telemetry init.
    - Commands that don't touch the daemon (`Init`, `List`, `Score`, `Logs`, `Daemon { cmd: Stop }`): unchanged — telemetry inits, command dispatches.
    - `--foreground` mode: the foreground process IS the daemon; runs telemetry init inline; no fork; no conflict.
  The parent's client-side telemetry and the grandchild's daemon-side telemetry therefore write to distinct run-dirs with zero risk of cross-contamination.
- `loopr daemon start --foreground` runs the same daemon loop in the foreground, holds the terminal, and exits on Ctrl-C. Same file artifacts as background mode (pid, socket, version); cleaned up on exit.
- `loopr daemon stop` sends `SIGTERM` to the PID in `.loopr/daemon.pid`, polls for the process to exit (up to ~3s), escalates to `SIGKILL` on timeout, removes the pid/socket/version/run-id files. A graceful daemon cleans up its own files during shutdown: a `tokio::signal`-driven watcher task inside the daemon awaits SIGTERM/SIGINT as an async value, then sets `ctx.shutting_down = true` and wakes `ctx.shutdown_notify`; the accept loop and in-flight handlers see the notification, exit, and `daemon_main` flushes the `telemetry::Guard` and calls `sentinel::clean(target)` before returning. `stop`'s post-SIGKILL fallback cleanup exists only for the ungraceful path.
- `loopr daemon status`:
    - If `.loopr/daemon.pid` is absent or stale (process gone): prints "no daemon running" to stdout, exits 0.
    - If daemon is running: connects, performs the handshake, sends `system.status`, prints the returned pid / started-at / active-plans / active-works (active counts are 0 in Stage 4), exits 0.
    - On handshake version mismatch: prints the mismatch message to stderr, exits non-zero.
- `loopr plan "x"` (and every other future client-side subcommand) connects-or-forks on first use. Semantics:
    - If `.loopr/daemon.pid` exists and the PID is alive AND the version file matches the binary's version AND the socket is bindable by us as a client (`UnixStream::connect` succeeds): use it.
    - If the PID is stale, or the version file mismatches, or the socket is missing: clean up stale files, double-fork a fresh daemon, wait (up to `START_TIMEOUT_SECS = 3`, polling every `POLL_INTERVAL_MS = 100`) for the new socket to appear, then connect.
    - Once connected: handshake, send the subcommand's request, receive the response (or receive the stream of events that end with the response), return. For Stage 4, `plan "x"` sends a placeholder request (§API Design) and on seeing `RpcError::MethodNotFound` returns `LooprError::StageUnimplemented { stage: 5, subcommand: "plan" }`. The round-trip still happens; the daemon's log still shows the request.
- The daemon's accept loop is single-task-per-connection via `tokio::spawn`. Each connection runs `handle_client`: reads `DaemonRequest` lines via `Framed<UnixStream, LinesCodec>::new_with_max_length(ipc::MAX_LINE_BYTES)`, dispatches via `ipc::Method::try_from(&req)` into an exhaustive match, writes `DaemonResponse` lines back. Broadcast `DaemonEvent`s (future stages emit; Stage 4 defines the channel but never fires one) forward to every connected client via a `tokio::sync::broadcast` channel (v4 pattern).
- Handshake is mandatory per connection. The first request MUST be `system.handshake`; any other method before handshake returns `RpcError::InvalidRequest("handshake required before: {method}")` and does NOT advance connection state. On handshake, if the client's advertised `protocol_version != ipc::PROTOCOL_VERSION`, daemon returns `RpcError::protocol_version_mismatch(client, daemon)` and closes the connection.
- Malformed bytes on the daemon's inbound stream (`decode_request_line` returns `Err`): log the failure at `warn!` level via the daemon's telemetry subscriber, close the connection (drop the `Framed`), do NOT synthesize a response. Symmetric on the client side for `decode_line` failures: log, drop the connection, surface `ClientError::Disconnected` to the caller.
- Every daemon-received request is logged at `info!` with fields `request_id = req.id`, `method = req.method`, `peer = "client"`. The `conn_id` field is inherited automatically from the enclosing `ipc.connection` span opened in `accept_loop`. The daemon's run-level span hierarchy (Stage 2) puts `run_id` on every event automatically, so a single log line carries `run_id` + `conn_id` + `request_id` + `method` without any explicit threading.
- A test-only override mechanism allows the version-mismatch AC to run without a second binary build: `LOOPR_PROTOCOL_VERSION_OVERRIDE` (integer), read by `protocol_version_or_override()` in the client, gated behind `cfg!(debug_assertions)` so release binaries ignore it.
- The `ipc` crate's I/O-free invariant is preserved: `cargo tree -p ipc` still shows no `tokio` / `tokio_util` / runtime deps after Stage 4.
- Source-guard, target resolution, telemetry init, and the `-C <path>` flag keep working for every subcommand (client-side AND daemon-side — the daemon independently resolves its target from its parent's CWD or `-C` flag before forking, carries it through, and initializes its own telemetry at the target's `.loopr/runs/` path).

### Non-Goals

- **Any method beyond `system.handshake` and `system.status` handled server-side.** `plan.create`, `work.get`, `bundle.propose`, etc. are Stage 5+. Stage 4's server-side handler match is exhaustive over the two-variant `ipc::Method`; expanding `Method` in later stages produces compile errors here that Stage 5 resolves by adding arms.
- **Business logic.** No taskstore access, no Plan persistence, no decomposer call. The daemon's `ipc::Method::Status` handler returns hardcoded zeros for `active_plans`/`active_works` because `domain` is still empty and there's nothing to count.
- **Parallel client requests from different clients with id collisions across connections.** Correlation is connection-scoped (Stage 3 §Security bullet); Stage 4 honors that by never sharing a `Framed` across connections.
- **TUI.** Deferred per `docs/vision.md`. Stage 4's client model is "short-lived CLI invocation: connect, handshake, one request, response, disconnect." Long-lived bidirectional stream consumers (TUI, `loopr events tail`) are a future stage's concern.
- **Automatic daemon restart on config change.** The version-file mismatch path (`daemon_version != binary_version`) is handled by `SIGTERM` + auto-fork on next client invocation; there is no watcher that restarts the daemon when `.loopr/config.yml` is edited.
- **Socket permissions beyond the default `.loopr/` dir permissions.** The socket inherits the parent directory's mode; since `.loopr/` is under the user's target (typically `$HOME` or below), it's already owner-accessible. No explicit `chmod 0600` on the socket itself in Stage 4; revisit if the security review surfaces it.
- **Cross-host IPC.** Unix domain sockets only, always. No TCP, no TLS, no auth tokens. The socket is in a user-owned directory.
- **Graceful hand-off on version upgrade.** If the daemon binary is rebuilt while a daemon is running, the next client invocation that detects the version mismatch sends `SIGTERM` and forks a fresh daemon. In-flight requests on the old daemon are lost; the client retries against the new daemon. Full lossless hand-off is a later concern if it matters.
- **Log compaction / rotation of `.loopr/runs/`.** Inherited from Stage 2 non-goals.

### Acceptance Criteria

Each item is an assertable check. The design is Done when every assertion below is true.

- `loopr -C /tmp daemon start` returns exit 0 within ~3 seconds; the caller's shell regains control; `.loopr/socket`, `.loopr/daemon.pid`, `.loopr/daemon.version`, `.loopr/daemon.run-id` all exist under `/tmp/.loopr/`
- The PID in `.loopr/daemon.pid` refers to a live process whose `/proc/<pid>/status` reports `Name: loopr`
- The version in `.loopr/daemon.version` equals `GIT_DESCRIBE` (the value embedded in `loopr --version`)
- The run-id in `.loopr/daemon.run-id` names an existing directory under `.loopr/runs/`
- The daemon has allocated its own run directory under `/tmp/.loopr/runs/<daemon-run-id>/` with `events.log` + `loopr.log` non-empty and containing a `daemon.started` event
- `loopr -C /tmp daemon status` connects, handshakes, receives a `StatusResult`, prints it to stdout in a human-readable form (multi-line key: value), exits 0; the daemon's `events.log` (located via `.loopr/daemon.run-id` pointer, NOT the client's own run-dir) contains a line with `fields.method == "system.handshake"` and another with `fields.method == "system.status"`, both tagged with the same `conn_id`, both carrying the DAEMON's `run_id` in the enclosing span fields
- `loopr -C /tmp daemon stop` sends SIGTERM; within ~3 seconds the daemon exits; `.loopr/socket`, `.loopr/daemon.pid`, `.loopr/daemon.version`, `.loopr/daemon.run-id` are all removed; exit 0
- `loopr -C /tmp daemon stop` when no daemon is running exits 0 with stdout "no daemon running"
- `loopr -C /tmp plan "x"` with no daemon running: auto-forks daemon, connects, handshakes, sends a request, receives an `RpcError::MethodNotFound("plan.create")` response, exits non-zero with stderr containing "not yet implemented (earned at Stage 5)"; the daemon stays up and its log shows all three of handshake, the plan request, and the error response
- `loopr -C /tmp plan "x"` with an already-running daemon: reuses the socket, does NOT fork, same client-side error; daemon log shows the new conn_id and request
- A second `loopr -C /tmp daemon start` while one is already running exits 0 without re-forking (idempotent; checks PID file, verifies alive, no-ops)
- A `loopr -C /tmp daemon start` with a stale `.loopr/daemon.pid` (PID in file no longer alive): cleans up the stale pid/socket/version files, forks fresh
- A `loopr -C /tmp daemon start` where `.loopr/daemon.version` mismatches the binary's version: sends SIGTERM to the old daemon, waits for exit, forks fresh (silent restart per v4 precedent)
- A `loopr -C /tmp daemon start --foreground` while a background daemon is already running: exits non-zero with stderr "daemon already running at pid X; use `loopr daemon stop` first"; does not clobber the running daemon's state
- PID reuse protection: with a `.loopr/daemon.pid` pointing at a PID belonging to a non-loopr process (simulated via test fixture writing a known non-loopr PID), `is_daemon_alive` returns false (name-check rejects); `ensure_daemon_if_needed` treats the sentinel as stale and cleans up before forking
- Handshake version mismatch: `loopr -C /tmp daemon status` invoked with a client that advertises a bogus `protocol_version` (via `LOOPR_PROTOCOL_VERSION_OVERRIDE` test-only env var, see §Testing Strategy) gets back `RpcError::ProtocolVersionMismatch`; the daemon's log records the mismatch at `warn!` level; client exits non-zero
- Handshake-required-first: a client that sends `system.status` before `system.handshake` on a fresh connection gets back `RpcError::InvalidRequest`; daemon log records the violation at `warn!`; subsequent handshake on the SAME connection is still accepted (the connection isn't closed on a handshake-order violation — it's a soft rejection, not a protocol error; revisit only if abused)
- Daemon handles N concurrent connections without cross-correlation: 5 clients connect simultaneously, each sends a `system.status` request with distinct ids (1..=5), each receives exactly one response whose id matches; daemon log shows 5 distinct conn_ids
- Malformed input: a client sends `b"not json\n"` to the daemon; daemon logs the parse error at `warn!`, closes the connection; client sees disconnection and exits cleanly; daemon stays up
- Oversize input: a client sends a 2-MiB line; daemon logs `ParseError::LineTooLong` at `warn!`, closes the connection; daemon stays up
- `cargo tree -p ipc` shows no tokio / tokio_util / runtime deps (I/O-free invariant preserved)
- `cargo tree -p loopr` adds `tokio` (features = ["full"]), `tokio_util` (features = ["codec"]), `libc` as new deps; no other unnecessary deps creep in
- `otto ci` inside `crates/loopr` is green; `otto ci` at the workspace root is green

## Proposed Solution

### Overview

`loopr` splits into two clearly-scoped submodules for Stage 4: `daemon/` (the process-lifecycle layer — fork, PID file, signal handling, run loop) and `transport/` (the IPC layer — socket bind, accept, per-connection handler, client-side connect-and-handshake). Both are used by `lib::run`'s dispatch, not by each other; the daemon owns the transport, not the other way around.

**Mental model (client side):**

```
loopr plan "x"
  │
  ├─▶ resolve target, source-guard
  │
  ├─▶ daemon::ensure_daemon_if_needed(target)   ◀── BEFORE parent telemetry init
  │     └─ forks the daemon if needed (parent continues; grandchild exits lib::run)
  │
  ├─▶ telemetry::init(target, client_run_id)    ◀── client's own subscriber
  │
  ├─▶ transport::connect_or_wait(target)
  │     └─ polls socket (daemon may still be starting if we just forked it)
  │
  ├─▶ client.handshake()                        (sends HandshakeParams { PROTOCOL_VERSION })
  │
  ├─▶ client.request_raw("plan.create", json!({"goal": goal}))
  │         └───────────────────▶ daemon ──▶ RpcError::MethodNotFound("plan.create")
  │
  └─▶ map to LooprError::StageUnimplemented { stage: 5 } ──▶ exit non-zero
```

**Mental model (daemon side):**

```
ensure_daemon(target)
  │
  ├─▶ libc::fork ──▶ parent returns to client caller
  │
  └─▶ child: setsid, second fork, redirect stdio
         │
         └─▶ grandchild: tokio::runtime::new().block_on(daemon_main(target))
                │
                ├─ LOCK-ACQUIRE PHASE (no cleanup on failure) ─────────────┐
                │  ├─▶ sentinel::write_pid with create_new(true)           │
                │  │     └─ if AlreadyExists: return LockLost              │
                │  │        (grandchild process::exit(0) silently;         │
                │  │         winner's files are NOT touched)               │
                │  ├─▶ sentinel::write_version                             │
                │  └─▶ sentinel::write_run_id                              │
                ├───────────────────────────────────────────────────────────┘
                │
                ├─ ACTIVE-DAEMON PHASE (cleanup on exit) ──────────────────┐
                │  ├─▶ allocate RunId                                      │
                │  ├─▶ telemetry::init(target, run_id, filter) -> Guard    │
                │  │     (safe: lib::run hoist means parent never          │
                │  │      called set_global_default; COW flag is false)    │
                │  ├─▶ if socket.exists() { remove_file(&socket) }         │
                │  ├─▶ bind .loopr/socket (UnixListener)                   │
                │  ├─▶ spawn signal-watcher task                           │
                │  │     └─ on SIGTERM/SIGINT: shutting_down=true;         │
                │  │        shutdown_notify.notify_waiters()               │
                │  ├─▶ build DaemonContext (Arc); run accept_loop(...)     │
                │  │        │                                              │
                │  │        └─▶ for each accept(): spawn handle_client     │
                │  │                                                       │
                │  └─▶ accept_loop exits: drop Guard; sentinel::clean      │
                └───────────────────────────────────────────────────────────┘
                │
                └─▶ process::exit(0)
```

The rule-of-thumb separation: **`daemon` owns process lifetime and filesystem sentinels. `transport` owns bytes-on-the-wire and `ipc::Method` dispatch. Neither knows about the other's internals; they share a `DaemonContext` struct that holds `run_id`, `started_at`, `pid`, the broadcast bus, the shutdown flag, and the shutdown-notify wakeup.**

### Architecture

```
crates/loopr/src/
├── main.rs                     (unchanged; thin shell)
├── lib.rs                      + dispatch wires DaemonCmd and connect-or-fork for client cmds
├── cli.rs                      (unchanged; DaemonCmd already defined in Stage 1)
├── error.rs                    + LooprError::{ClientIo, DaemonStartup, HandshakeFailed, Rpc}
├── guard.rs                    (unchanged)
├── target.rs                   (unchanged)
├── logs.rs                     (unchanged)
├── daemon.rs                   NEW — module entry: pub fn ensure_daemon, daemon_main, signal handling
├── daemon/                     NEW — submodules of `daemon`
│   ├── fork.rs                 double-fork primitive (libc-based)
│   ├── sentinel.rs             pid / version / run-id / socket file read/write/cleanup
│   └── context.rs              DaemonContext (run_id, started_at, pid, bus, shutdown flag)
├── transport.rs                NEW — module entry: connect_or_wait, const POLL_INTERVAL_MS / START_TIMEOUT_SECS
├── transport/                  NEW — submodules of `transport`
│   ├── server.rs               accept_loop + handle_client per-connection task
│   ├── client.rs               IpcClient: connect, handshake, request/request_raw
│   └── handler.rs              exhaustive match on ipc::Method → DaemonResponse
└── tests.rs                    + Stage 4 integration tests
```

Per `rules/rust.md`: single-word filenames, Rust 2018+ module style (module entry point is `foo.rs`, submodules live in `foo/` alongside it — not `foo/mod.rs`). Tests in a sibling `tests.rs` per the user-memory rule against bottom-of-file `mod tests` blocks. Both `daemon` and `transport` ARE the module entry; the files under `daemon/` and `transport/` are `pub(crate)` submodules re-exported through the entry `.rs`.

### Data Model

```rust
// crates/loopr/src/daemon/context.rs

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use tokio::sync::{broadcast, Notify};

use ipc::DaemonEvent;
use telemetry::RunId;

/// Shared state for the daemon run. Held in an `Arc` by the accept loop, each
/// connection handler task, and the signal handler. Values are set once at
/// startup and read-only thereafter; the only mutable cell is `shutting_down`.
pub struct DaemonContext {
    pub target: PathBuf,
    pub run_id: RunId,
    pub started_at: chrono::DateTime<chrono::Local>,
    pub pid: u32,
    /// Broadcast bus for `DaemonEvent`s. Stage 4 defines the channel but
    /// never fires an event. Stage 5+ fires on record transitions.
    pub events: broadcast::Sender<DaemonEvent>,
    /// Set to `true` by the signal-watcher task or by an in-process shutdown
    /// request. `accept_loop` and every `handle_client` read it to decide
    /// whether to exit.
    pub shutting_down: Arc<AtomicBool>,
    /// Async-friendly wakeup. A single `tokio::signal`-driven task awaits
    /// SIGTERM/SIGINT; on signal it sets `shutting_down = true` and calls
    /// `shutdown_notify.notify_waiters()`, which wakes every consumer that
    /// called `shutdown_notify.notified().await`. This avoids polling.
    /// NOTE: the signal-watcher runs as a `tokio::spawn` task, not as a
    /// POSIX signal handler - `tokio::signal::unix::signal(SIGTERM)?.recv()`
    /// delivers the signal AS an async value. No `async-signal-safe`
    /// constraints apply because we never touch tokio from a true signal
    /// handler context.
    pub shutdown_notify: Arc<Notify>,
}
```

```rust
// crates/loopr/src/error.rs (additions)

#[derive(Error, Debug)]
pub enum LooprError {
    // ...existing variants unchanged...

    /// Client-side transport failure (socket gone, connection dropped mid-RPC,
    /// codec error). Distinct from `DaemonStartup` because the daemon was
    /// assumed to be alive — something happened on the wire.
    #[error("ipc client error: {0}")]
    ClientIo(String),

    /// The daemon failed to start — either the fork failed, or the socket
    /// never appeared within `START_TIMEOUT_SECS`. Surfaces as a clean exit,
    /// not a panic.
    #[error("daemon startup failed: {0}")]
    DaemonStartup(String),

    /// The grandchild lost the PID-file race against another concurrently-
    /// starting grandchild. Internal-only: `daemon::run_grandchild` catches
    /// this, silently `process::exit(0)`, and does NOT run cleanup (the race
    /// winner still needs its sentinel files intact). Never surfaces to a
    /// human.
    #[error("daemon pid lock already held by another grandchild")]
    LockLost,

    /// Handshake negotiation produced a protocol-version mismatch or an
    /// unexpected response shape.
    #[error("handshake failed: {0}")]
    HandshakeFailed(String),

    /// The daemon replied with an `RpcError`. Carries the typed variant so
    /// `main.rs` can map well-known variants to specific exit codes (e.g.
    /// `MethodNotFound("plan.create")` → `StageUnimplemented { stage: 5 }`).
    #[error("daemon returned rpc error: {0}")]
    Rpc(#[from] ipc::RpcError),
}
```

```rust
// crates/loopr/src/transport/client.rs

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

use ipc::{DaemonEvent, DaemonRequest, DaemonResponse, IpcMessage, MethodName, PROTOCOL_VERSION, RpcError};

/// Short-lived IPC client: connect, handshake, one-shot request, events
/// collected during the wait. Drop on exit.
pub struct IpcClient {
    framed: Framed<UnixStream, LinesCodec>,
    next_id: AtomicU64,
}

impl IpcClient {
    pub async fn connect(socket: &Path) -> Result<Self, LooprError>;
    pub async fn handshake(&mut self) -> Result<ipc::HandshakeResult, LooprError>;
    /// Typed request: the common path. `MethodName` round-trips to the wire
    /// via `strum::Display`, so adding a variant to `MethodName` in a future
    /// stage picks up here for free.
    pub async fn request(
        &mut self,
        method: MethodName,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError>;
    /// Untyped request: Stage-4-only escape hatch for sending a method name
    /// that has not yet been promoted into `MethodName`. Used by the
    /// `loopr plan "x"` path to send "plan.create" before Stage 5 adds the
    /// variant; the daemon returns `RpcError::MethodNotFound` which the
    /// caller maps to `LooprError::StageUnimplemented`. Removed in Stage 5.
    pub async fn request_raw(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError>;
}
```

### API Design

**Client-side connect-with-wait (`transport::connect_or_wait`):**

`lib::run` is the sole fork authority (via synchronous `ensure_daemon_if_needed` hoisted above telemetry init). By the time any client code reaches the async layer inside `rt.block_on`, the fork decision has been made and the parent has returned. The async function only connects, with a retry/poll budget for the case where the daemon is still starting up.

```rust
pub async fn connect_or_wait(target: &Path) -> Result<IpcClient, LooprError> {
    let socket = target.join(".loopr").join("socket");

    // Poll for socket up to START_TIMEOUT_SECS at POLL_INTERVAL_MS cadence.
    // `lib::run`'s pre-telemetry hoist guarantees that if a fork was needed,
    // it has already happened; we are polling for the grandchild's `bind`.
    let deadline = Instant::now() + Duration::from_secs(START_TIMEOUT_SECS);
    loop {
        if let Ok(client) = IpcClient::connect(&socket).await {
            return Ok(client);
        }
        if Instant::now() > deadline {
            return Err(LooprError::DaemonStartup(
                format!("socket never appeared at {}", socket.display())
            ));
        }
        tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}
```

The previous draft included a fallback `daemon::ensure_daemon` call inside this function. That was wrong: it would `libc::fork` from inside a multithreaded Tokio process (POSIX-unsafe; grandchild deadlock risk) AND the grandchild would inherit the parent's already-installed telemetry subscriber, crashing on its own `telemetry::init` with `AlreadyInitialized`. The architect flagged this as the "Hardest Question"; the answer is that `connect_or_wait` MUST NOT fork — `lib::run` is the sole authority.

**Handshake (client):**

```rust
pub async fn handshake(&mut self) -> Result<ipc::HandshakeResult, LooprError> {
    let params = ipc::HandshakeParams { protocol_version: protocol_version_or_override() };
    let (resp, _) = self.request(MethodName::SystemHandshake, to_value(params)?).await?;
    if let Some(err) = resp.error {
        return Err(LooprError::HandshakeFailed(err.to_string()));
    }
    let result: ipc::HandshakeResult = serde_json::from_value(resp.result.unwrap())
        .map_err(|e| LooprError::HandshakeFailed(format!("bad result: {e}")))?;
    Ok(result)
}
```

`protocol_version_or_override()` reads the `LOOPR_PROTOCOL_VERSION_OVERRIDE` env var if set (test-only escape hatch for the mismatch AC), else returns `ipc::PROTOCOL_VERSION`. Not wired into public CLI surface.

**Daemon accept loop (`transport::server::accept_loop`):**

```rust
pub async fn accept_loop(
    listener: UnixListener,
    ctx: Arc<DaemonContext>,
) -> Result<(), std::io::Error> {
    use tracing::Instrument;
    loop {
        // Symmetric with `handle_client`'s loop-top check: `Notify::notify_waiters`
        // is non-sticky, so if the signal watcher fires during the microseconds
        // between the last `select!` return and the next iteration (e.g., while
        // we were spawning a connection task), we'd miss the wakeup and block on
        // `accept()` forever. The atomic load catches that case.
        if ctx.shutting_down.load(Ordering::Relaxed) { break Ok(()); }
        tokio::select! {
            biased;
            _ = shutdown_notified(&ctx) => {
                info!("accept_loop: shutdown notified");
                break Ok(());
            }
            accept = listener.accept() => {
                let (stream, _) = accept?;
                let conn_id = uuid::Uuid::new_v4();
                let span = tracing::info_span!("ipc.connection", conn_id = %conn_id);
                let ctx = ctx.clone();
                // `.instrument(span)` is the async-aware attach. `.entered()`
                // would drop the guard at every `.await` and lose the span;
                // `Future::instrument` re-enters the span on each poll.
                tokio::spawn(handle_client(stream, ctx).instrument(span));
            }
        }
    }
}
```

**Per-connection handler (`transport::server::handle_client`):**

```rust
async fn handle_client(stream: UnixStream, ctx: Arc<DaemonContext>) {
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(ipc::MAX_LINE_BYTES));
    let mut event_rx = ctx.events.subscribe();
    let mut state = HandshakeState::Pending;

    loop {
        // Fast check at loop top: `Notify::notify_waiters()` only wakes tasks
        // currently awaiting. If shutdown fires while this handler is mid-
        // `framed.send`, the notify can be dropped and the handler misses
        // the wakeup. The atomic load catches that case; the notify is the
        // fast path for idle handlers.
        if ctx.shutting_down.load(Ordering::Relaxed) { break; }
        // `biased` gives shutdown priority: when the daemon is shutting down
        // we drop every in-flight handler before accepting the next select arm.
        tokio::select! {
            biased;
            _ = shutdown_notified(&ctx) => break,
            line = framed.next() => match line {
                Some(Ok(line)) => {
                    // LinesCodec has already stripped the trailing newline.
                    // Encoding uses `serde_json::to_string` + `framed.send(&str)` —
                    // `LinesCodec` adds its own `\n`; calling `ipc::encode_line`
                    // would double up (see "Note on encode_line" below).
                    match ipc::decode_request_line(line.as_bytes()) {
                        Ok(req) => {
                            let response = handler::dispatch(&req, &mut state, &ctx).await;
                            let close_after = is_protocol_version_mismatch(&response);
                            let line_out = serde_json::to_string(&response)
                                .expect("ipc response must serialize");
                            if framed.send(line_out).await.is_err() { break; }
                            // Stage 3 "Forward-Compatibility" contract: close on
                            // version mismatch so the client can't keep trying.
                            if close_after { break; }
                        }
                        Err(e) => {
                            warn!("parse error: {e}; closing connection");
                            break;  // NO fake-id synthesis per Stage 3 contract.
                        }
                    }
                }
                Some(Err(e)) => { warn!("codec error: {e}; closing"); break; }
                None => break,  // client disconnected
            },
            event = event_rx.recv() => match event {
                Ok(event) => {
                    let line_out = serde_json::to_string(&event)
                        .expect("ipc event must serialize");
                    if framed.send(line_out).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }
    }
}

/// Future that resolves as soon as `ctx.shutting_down` flips to true. A
/// `Notify` on the DaemonContext woken by the signal handler gives a proper
/// async wakeup without polling.
async fn shutdown_notified(ctx: &DaemonContext) {
    ctx.shutdown_notify.notified().await;
}

fn is_protocol_version_mismatch(resp: &DaemonResponse) -> bool {
    matches!(&resp.error, Some(ipc::RpcError::ProtocolVersionMismatch(_)))
}
```

**Note on `ipc::encode_line` vs. `serde_json::to_string` + `framed.send`:** `ipc::encode_line` produces a complete NDJSON line with trailing `\n`, suitable for raw `AsyncWrite::write_all` usage and for the `ipc` crate's own round-trip tests. `LinesCodec::encode` appends its own `\n`; feeding it bytes already carrying `\n` produces a double-newline on the wire (empty line between messages, which the client's `LinesCodec` reads as a zero-byte line that `decode_line` rejects). When using `Framed<_, LinesCodec>`, always go through `serde_json::to_string` directly. The client-side `IpcClient::request` + `request_raw` follow the same rule.

**Exhaustive dispatch (`transport::handler::dispatch`):**

```rust
pub enum HandshakeState { Pending, Complete }

pub async fn dispatch(
    req: &DaemonRequest,
    state: &mut HandshakeState,
    ctx: &DaemonContext,
) -> DaemonResponse {
    let method = match ipc::Method::try_from(req) {
        Ok(m) => m,
        Err(rpc_err) => return DaemonResponse::err(req.id, rpc_err),
    };

    // Handshake ordering enforcement (Stage 3 contract).
    match (&method, &state) {
        (ipc::Method::Handshake(_), _) => {}  // allowed any time (idempotent)
        (_, HandshakeState::Pending) => {
            warn!(method = %req.method, "rejected: handshake required first");
            return DaemonResponse::err(req.id, RpcError::InvalidRequest(
                format!("handshake required before: {}", req.method)
            ));
        }
        _ => {}
    }

    // Exhaustive match: adding a variant to ipc::Method is a compile error here.
    match method {
        ipc::Method::Handshake(params) => handle_handshake(req.id, params, state),
        ipc::Method::Status             => handle_status(req.id, ctx),
    }
}
```

`handle_handshake` verifies `params.protocol_version == ipc::PROTOCOL_VERSION`, sets `*state = Complete`, returns `DaemonResponse::ok(req.id, to_value(HandshakeResult { protocol_version, daemon_version })?)`. `handle_status` returns `DaemonResponse::ok(req.id, to_value(StatusResult { started_at, pid, active_plans: 0, active_works: 0 })?)`.

**`loopr plan "x"` client path (lib::run dispatch):**

```rust
Command::Plan { goal } => {
    // Stage 4: exercise the transport end-to-end, then map the daemon's
    // MethodNotFound response to StageUnimplemented for a clean UX. This
    // body is replaced in Stage 5 with a typed `MethodName::PlanCreate`
    // call once the method joins the enum.
    let rt = tokio::runtime::Runtime::new().map_err(|e| LooprError::ClientIo(e.to_string()))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake().await?;
        // `request_raw` because `plan.create` is not yet a `MethodName`
        // variant (that lands in Stage 5, paired with the handler arm).
        // Daemon's `Method::try_from` returns `RpcError::MethodNotFound`
        // for unknown method strings; mapping that to StageUnimplemented
        // preserves the Stage 1/2/3 UX.
        let (resp, _) = client.request_raw(
            "plan.create",
            serde_json::json!({ "goal": goal }),
        ).await?;
        if let Some(err) = resp.error {
            if matches!(err, RpcError::MethodNotFound(_)) {
                return Err(LooprError::StageUnimplemented { stage: 5, subcommand: "plan" });
            }
            return Err(LooprError::Rpc(err));
        }
        Ok(())
    })
}
```

**`IpcClient::request` signature note.** The v4 client `request(method: impl Into<String>, ...)` took a string; the Stage 4 client accepts `MethodName` for the typed path AND `&str` for the "send a method the daemon doesn't know yet" case (via a separate `request_raw` method). This preserves type safety on the common path while letting Stage 4 exercise unknown-method behavior without a pre-emptive `MethodName` variant addition. When Stage 5 adds `MethodName::PlanCreate`, the `plan "x"` client path switches to the typed variant.

### Implementation Plan

Six phases, each a single-commit milestone on branch `v5`. Each phase ends with `otto ci` green inside `crates/loopr` and at the workspace root.

#### Phase 1: Dependencies + module scaffolding

**Model:** sonnet

- `cargo add --package loopr tokio --features full`
- `cargo add --package loopr tokio-util --features codec`
- `cargo add --package loopr libc`
- `cargo add --package loopr uuid --features v4`
- Create `src/daemon.rs` (module entry) and `src/daemon/{fork,sentinel,context}.rs` with module headers and empty function stubs. `daemon.rs` declares `mod fork; mod sentinel; mod context;` and re-exports `ensure_daemon` + `daemon_main` + `DaemonContext`.
- Create `src/transport.rs` (module entry) and `src/transport/{server,client,handler}.rs` with module headers and empty function stubs. `transport.rs` declares the submodules, exposes `POLL_INTERVAL_MS` / `START_TIMEOUT_SECS` consts, and exports `connect_or_wait` + `IpcClient`.
- Add `pub mod daemon;` + `pub mod transport;` to `lib.rs`.
- Add `LooprError::{ClientIo, DaemonStartup, HandshakeFailed, Rpc}` variants to `error.rs`.
- `cargo check -p loopr --all-targets` green; `cargo test -p loopr` green (nothing new tested; existing Stage 1/2/3 tests still pass).

#### Phase 2: Sentinel files (pid / version / socket)

**Model:** sonnet

- `src/daemon/sentinel.rs`: pure filesystem helpers — `read_pid`, `write_pid` (uses `OpenOptions::new().create_new(true)` for atomic claim), `write_version(path, version)`, `write_run_id(path, run_id)`, `is_daemon_alive(pid_file)` (two-step: first `kill(pid, 0)` for liveness, then on Linux read `/proc/<pid>/comm` or on macOS shell out to `ps -p <pid> -o comm=` and verify the process name is `loopr` — PID reuse protection per the Risk table), `version_matches(version_file)`, `kill_stale(pid_file)` (SIGTERM + poll up to `STOP_TIMEOUT_SECS`, escalate to SIGKILL), `clean(target)` (remove pid, socket, version, run-id files — idempotent).
- Unit tests with `tempfile::TempDir` covering each helper on both present-and-valid and present-but-stale inputs, including the atomic-claim race on `write_pid` (two tasks try to write; one wins, one errors).
- No runtime required; these are sync filesystem calls.

#### Phase 3: Double-fork primitive + DaemonContext

**Model:** opus

- `src/daemon/fork.rs`: `pub fn double_fork() -> ForkOutcome`. `ForkOutcome::Parent` is what `ensure_daemon` returns to the client; `ForkOutcome::Daemon` is what the grandchild gets. Inside Daemon, caller MUST set up the tokio runtime (fork is unsafe on a multithreaded process; runtime creation comes AFTER fork).
- `src/daemon/context.rs`: `DaemonContext { target, run_id, started_at, pid, events, shutting_down, shutdown_notify }` per §Data Model.
- `src/daemon.rs` (module entry):
    - `pub fn ensure_daemon(target: &Path) -> Result<(), LooprError>` — unconditional parent-side entry point (for `daemon start`). Checks sentinel; calls `double_fork`; on `Parent` returns to client caller; on `Daemon` never returns (creates a tokio runtime, calls `block_on(daemon_main(target))`, then `process::exit(...)`; the grandchild's stack never unwinds back through `double_fork` into `lib::run`).
    - `pub fn ensure_daemon_if_needed(target: &Path) -> Result<(), LooprError>` — parent-side entry point for client commands that need a live daemon. If sentinel shows an alive + version-matching daemon, returns `Ok(())` immediately. Otherwise cleans stale sentinel state and calls the same `double_fork` path as `ensure_daemon`. Exists as a separate function because `lib::run` must call it BEFORE its telemetry init for the client commands that can auto-fork.
    - `pub async fn daemon_main(target: PathBuf) -> Result<(), LooprError>` — grandchild's async body. Split into two phases to keep the lock-loser exit path clean:
        1. **Lock-acquire phase (no cleanup on failure):**
           - `sentinel::write_pid(&pid_file, pid)` with `OpenOptions::new().create_new(true)` — atomic claim. If it returns `AlreadyExists`, we lost a race with another grandchild; return `LockLost` immediately. `daemon::run_grandchild` catches `LockLost`, silently `process::exit(0)`, and DOES NOT call `sentinel::clean(target)` (the winner's files must stay intact).
           - Only after the PID lock succeeds: `sentinel::write_version`, `sentinel::write_run_id`.
        2. **Active-daemon phase (cleanup on exit):**
           - Allocate `RunId`, call `telemetry::init` (grandchild's OWN subscriber — safe because `lib::run`'s hoist ensures the parent never called `set_global_default`, so the "already set" flag is false in the grandchild's COW'd memory).
           - `if socket.exists() { std::fs::remove_file(&socket)?; }` UNCONDITIONALLY before `UnixListener::bind` — a stale socket file (left behind by SIGKILL'd previous daemon, or hand-deleted PID without hand-deleting socket) would otherwise produce `EADDRINUSE`. The PID lock we already hold is the only authority that matters; the socket file on disk is just a side-effect.
           - Bind listener, spawn the signal-watcher task (`tokio::signal::unix::signal(SignalKind::terminate())?.recv().await` → set `shutting_down` + `notify_waiters`), run `accept_loop`.
           - On normal shutdown: flush `telemetry::Guard`, call `sentinel::clean(target)` (safe because we are the winner; we hold the lock).
    - `lib::run` control flow: first match on the command. `Daemon { Start { foreground: false } }` → `ensure_daemon` + parent-exit. `Plan | Decompose | Execute | Integrate | Daemon { Status }` → `ensure_daemon_if_needed` BEFORE telemetry init. Every other command → straight to telemetry init + dispatch (the existing Stage 2 path).
- Opus because double-fork semantics + signal-handler-safety + "no tokio before fork" + the telemetry-init ordering are tricky integrations where a mistake is hard to diagnose after the fact.

#### Phase 4: Server transport + handler dispatch

**Model:** sonnet

- `src/transport/server.rs`: `bind_listener(socket: &Path)`, `accept_loop(listener, ctx)`, `handle_client(stream, ctx)`. The `handle_client` body per §API Design.
- `src/transport/handler.rs`: `dispatch(req, state, ctx) -> DaemonResponse`. Exhaustive match on `ipc::Method`; handlers for `Handshake` and `Status`. Handshake-ordering enforcement per §API Design.
- Unit tests: handler-only tests that construct a `DaemonRequest` + `HandshakeState` in-memory, call `dispatch`, assert the `DaemonResponse`. No socket involved.
- Integration test: bind listener → connect with `UnixStream` directly (no `IpcClient` yet) → send bytes → read response → assert.

#### Phase 5: Client transport + connect-or-fork

**Model:** sonnet

- `src/transport/client.rs`: `IpcClient::connect`, `handshake`, `request`, `request_raw`.
- `src/transport.rs` (module entry): `pub async fn connect_or_wait(target)` per §API Design, `POLL_INTERVAL_MS` / `START_TIMEOUT_SECS` consts, re-exports `IpcClient` + `accept_loop`.
- Wire `DaemonCmd::{Start, Stop, Status}` to real bodies in `lib.rs`:
    - `Start` calls `daemon::ensure_daemon(target)`, prints the pid, exits 0.
    - `Start --foreground` calls `daemon::daemon_main(target).await` directly (no fork).
    - `Stop` reads pid file, sends SIGTERM, polls for exit (escalates to SIGKILL on timeout), cleans up files.
    - `Status` calls `transport::connect_or_wait`, `client.handshake()`, `client.request(MethodName::SystemStatus, json!({}))`, prints result.
- Wire `Command::Plan` to its Stage 4 body per §API Design.
- Integration test: spawn daemon (via `ensure_daemon`), connect via `IpcClient`, handshake, request, teardown.

#### Phase 6: E2E + stabilization + CI

**Model:** sonnet

- `crates/loopr/tests/daemon_stage4.rs` (new E2E file, `assert_cmd`-based):
    - Each test uses a fresh `TempDir` as the target, invokes `loopr -C <tmp> daemon start`, asserts files, sends `loopr -C <tmp> daemon status`, asserts stdout, invokes `loopr -C <tmp> daemon stop`, asserts cleanup.
    - Test for stale pid recovery, version mismatch, malformed input, concurrent clients.
- Bump to `v0.5.6` per the v5 per-branch-bump override (`memory/project-v5-branch-versioning.md`).
- `otto ci` inside `crates/loopr` green; `otto ci` at workspace root green.

## Alternatives Considered

### Alternative 1: Single-fork daemon (no setsid, no second fork)

- **Description:** Fork once, parent returns, child runs daemon. Skip `setsid` + second fork.
- **Pros:** Simpler to write and reason about (3 lines of libc instead of ~30).
- **Cons:** Single-fork daemon retains the controlling terminal. If the user's shell is a process-group leader and the user hits Ctrl-C in the original shell after the daemon starts, SIGINT propagates to the daemon's process group and kills it. Double-fork (fork → setsid → fork) is the POSIX-correct daemonization dance precisely to sever process-group membership.
- **Why not chosen:** v4 uses double-fork and worked in production. The added complexity is ~20 lines in `fork.rs` and eliminates a whole class of "why did my daemon die when I closed the terminal" bugs.

### Alternative 2: Use the `daemonize` crate

- **Description:** Add `daemonize = "0.5"` as a dep, call `Daemonize::new().start()`.
- **Pros:** No hand-written libc code; documented API.
- **Cons:** The crate abstracts away details we need visibility into (when exactly is the pid file written? what happens if the second fork fails mid-way?). v4 built its own for this reason. One more dep that might not be maintained (last release predates most other crates in the workspace).
- **Why not chosen:** The 30 lines we'd hand-write are lifted verbatim from v4 (which has production miles on them). Avoid the dep.

### Alternative 3: JSON-RPC library (jsonrpsee, rmp-rpc, etc.)

- **Description:** Use an off-the-shelf JSON-RPC library for the transport instead of hand-coding `Framed<UnixStream, LinesCodec>` around `ipc::Method`.
- **Pros:** Batched requests, streaming responses, built-in error codes.
- **Cons:** Most JSON-RPC libs are HTTP-first; Unix-socket transport is an afterthought at best. All of them want to own the message type definitions, which collides with `ipc` owning them. Stage 3's `ipc` protocol is JSON-RPC-**compatible** (same error codes, same envelope shape) but not JSON-RPC-conformant (v4's unsolicited events don't fit the spec).
- **Why not chosen:** v4's hand-rolled approach shipped and worked. The value of a framework would be in batching / streaming, both of which are Stage 4 non-goals. Adding a dep that then has to be defeated in the specific ways v5 diverges from JSON-RPC is a net negative.

### Alternative 4: One accept loop, one connection at a time (no `tokio::spawn` per connection)

- **Description:** Serialize connections — only one client talks to the daemon at a time. Simpler; no concurrent-connection races.
- **Pros:** Eliminates the "5 clients with overlapping ids" class of bugs by construction. No broadcast bus needed until Stage 7.
- **Cons:** A long-running client (future TUI) would block every other client. Even in Stage 4, `loopr daemon status` during a `loopr plan "x"` in-flight would queue behind it.
- **Why not chosen:** v4's model is per-connection tasks; the broadcast bus already exists in the design for events (Stage 7+). Sequential-only is an easier-to-write version that we'd tear out in three stages. Pay the complexity now, in a stage where concurrency is mechanically easy to test.

### Alternative 5: Daemon on per-invocation (no persistent daemon)

- **Description:** Each `loopr plan "x"` invocation does the whole pipeline inline; no background daemon at all. Stage 4 goes away.
- **Pros:** Skips the whole process-lifetime and signal-handling layer.
- **Cons:** Defeats the reactive-daemon thesis of v5 (§Observability, §loopr driver). Agents are long-lived; ralph loops run for minutes; you can't fit that into a per-CLI-invocation process without losing state between invocations, which means re-reading TaskStore on every CLI call, which is the exact pattern v3 discarded for good reason.
- **Why not chosen:** This is rewriting v1, which has been rewritten. The vision is clear.

### Alternative 6: Socket-activated daemon (systemd-style)

- **Description:** systemd (or a launchd equivalent) pre-binds the socket and hands it to the daemon on first connection.
- **Pros:** Fork is kernel's problem, not ours. Clean start-on-demand.
- **Cons:** Couples loopr to systemd/launchd. loopr targets any user's machine, including containers and bare shells; systemd isn't universally available. Also, "socket on first connection" means cold-start latency is paid on every first invocation, exactly when the user is waiting.
- **Why not chosen:** Cross-platform requirement. Also: the whole point of `connect-or-fork` is that the client doesn't care whether this is the first invocation or the thousandth; the daemon is transparent to the user. Socket activation would add a case ("oh, it's systemd-managed here") that has to be documented.

## Technical Considerations

### Dependencies

- `tokio` with `features = ["full"]`. Full because we use `net`, `sync::broadcast`, `sync::oneshot`, `signal`, `time::sleep`, `task::spawn`, `runtime::Runtime`. Being specific would save a few KB in binary size; not worth the audit cost.
- `tokio-util` with `features = ["codec"]` for `LinesCodec` + `Framed`. Matches `ipc`'s "Stage 4 consumption" note.
- `libc` for `fork` / `setsid` / `kill` / `waitpid` / `open("/dev/null")`. No higher-level wrapper because we need the specific double-fork dance.
- `uuid` with `features = ["v4"]` for per-connection correlation ids in logs. Pure-random UUIDs are fine; we don't need ordered or timestamped ones.
- `chrono` already present.
- No async-std, no smol, no io-uring — just tokio.

### Performance

- **Socket accept:** ~1ms per connection on Linux domain sockets. Stage 4 sees single-digit connections per minute (human CLI use); irrelevant.
- **Per-request latency (handshake + status):** end-to-end measured from v4 is ~2ms. NDJSON encode + UnixStream write + decode + dispatch + encode + write is cheap.
- **Double-fork startup cost:** ~30-50ms on warm cache, ~100ms cold. The client's `START_TIMEOUT_SECS = 3` has generous headroom.
- **Broadcast bus capacity:** `broadcast::channel::<DaemonEvent>(64)` — 64 is the v4 value; sized for short lag spikes without dropping. Stage 4 never sends on it; capacity is future-proofing.
- **Memory:** per connection, a `Framed<UnixStream, LinesCodec>` holds a 1-MiB read buffer max. With 5 concurrent connections (the Stage 4 AC), worst-case memory is ~5 MiB — fine for any modern host.

### Security

- **Socket permissions:** inherited from `.loopr/` dir mode. User-owned dir → user-only socket access. No explicit chmod; no network exposure.
- **PID file races:** `write_pid` uses `OpenOptions::new().create_new(true).open(pid_path)` to atomically claim the lock; if it fails, another daemon is racing us and we lose. The losing daemon exits cleanly; no double-bind possible.
- **Stale PID attack surface:** an attacker who can write `.loopr/daemon.pid` (requires user-level write to the target) can point it at another process's PID, causing SIGTERM to be sent to that PID when `loopr daemon stop` is invoked. Not a Stage 4 concern because the attacker already has user-level write to the target; at that point much worse attacks are available.
- **Fork + tokio:** `fork()` is unsafe in a multithreaded process. The daemon parent does NOT create a tokio runtime before calling `ensure_daemon`. The grandchild creates its runtime AFTER `fork + setsid + fork`. `ensure_daemon` is called from `main` → `lib::run` → `dispatch` → `DaemonCmd::Start`; the whole path is single-threaded up to that point. Signal handling is installed INSIDE the grandchild's tokio runtime via `tokio::signal::unix::signal(SignalKind::terminate())?` — not as a POSIX signal handler at the libc level — so there is no async-signal-safety concern for cleanup code.
- **libc safety:** each `unsafe` block in `fork.rs` is minimal and commented. The pattern is lifted verbatim from v4 where it has production miles.
- **NDJSON payload safety (architect design-review finding, reclassified as verified):** the architect raised a concern that a literal `0x0A` byte inside a JSON string payload would split the line prematurely under `LinesCodec`, severing the connection. Verified not a real risk: `serde_json::to_string` ASCII-escapes every control character (including `\n` → literal two-byte `\\n`) in string values per RFC 8259, and emits no structural whitespace at all in compact mode. Every byte in the output is either a JSON structural character or an ASCII-escaped content byte; the only `0x0A` on the wire is the terminator `LinesCodec` appends. Fuzz-testing with `{"message": "line1\nline2"}` round-trips cleanly and is included as a framing-safety unit test in Phase 4.

### Testing Strategy

- **Unit tests** (sync, `tempfile::TempDir`-scoped): sentinel file helpers, handshake state transitions, `dispatch` for each `ipc::Method` variant, `connect_or_wait` with mocked daemon presence.
- **Integration tests** (tokio, same-process daemon+client): spawn `daemon_main` on a task, connect, exercise handshake + status + concurrent + malformed. Use a test-only `DaemonContext::new_for_test()` that skips signal-handler installation (signal handlers in tests can hang the test harness).
- **E2E tests** (`assert_cmd`, separate process daemon via `loopr daemon start`): the AC list above, each a test.
- **Test-only env var `LOOPR_PROTOCOL_VERSION_OVERRIDE`:** read in `protocol_version_or_override()` to let the version-mismatch AC run without a second binary build. Gated to be read only if `cfg!(test)` OR `cfg!(debug_assertions)` — production release builds ignore the env var.

### Rollout Plan

- Phases 1-6 land as sequential commits on `v5` branch; CI green between each.
- `v0.5.6` tag after Phase 6 (per v5-specific branch versioning memory).
- No coexistence migration (Stage 1 and Stage 2 kept working through the Phase 1-5 transition because `DaemonCmd::Start` returned `StageUnimplemented` until Phase 5 wired it; existing `logs tail` / `logs runs` tests stay green throughout).
- Stage 5 consumption: Stage 5 adds `MethodName::PlanCreate` + the typed params / result structs to `ipc`, adds a `Method::PlanCreate` arm to `dispatch` (compile-error-driven — the exhaustive match forces Stage 5 to implement the handler), adds a taskstore write to the handler body, and updates the `plan "x"` client path to use `MethodName::PlanCreate` and expect `Ok` instead of `MethodNotFound`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Double-fork goes wrong on non-Linux (macOS has `proc` path differences) | Medium | Medium | `is_daemon_alive` uses `kill(pid, 0)` as the portable check; `/proc/<pid>` is Linux-only and used as an optimization when present. Stage 4 runs on Linux primarily; macOS works with the fallback path. |
| `tokio::task::spawn` inside `accept_loop` orphans handler tasks on shutdown | Medium | Low | Every `handle_client` has (a) a loop-top `ctx.shutting_down.load()` check for the "notify-miss" case where shutdown fires mid-`framed.send`, (b) a biased `tokio::select!` arm on `shutdown_notified(&ctx)` for the idle-handler case. Together they cover both branches: `notify_waiters` is only lossy for tasks not currently awaiting, and the atomic load picks those up on the next loop iteration. Drop of `ctx` on daemon exit also closes `events` broadcast as a third wakeup. |
| Signal handler races with tokio runtime shutdown | Low | High | Signals are consumed as async values via `tokio::signal::unix::signal(SIGTERM)?.recv().await` inside a dedicated watcher task — NOT from a POSIX signal-handler stack. Async-signal-safety is a non-issue. All cleanup happens in `daemon_main`'s tail after the runtime has processed the shutdown notification. |
| Version-file corruption leads to infinite fork loop (v0.5.6 detects v0.5.5 daemon, kills, forks, which writes v0.5.6, next client reads v0.5.6, matches, but there's a race) | Low | Medium | Version match is checked AFTER pid aliveness. A newly-forked daemon's first act is to write the version file. Race window is the ~50ms between double-fork return and version-file write. Within that window, a second client will see pid alive + version mismatch and SIGTERM. We accept this; the operation is idempotent. |
| Broadcast bus lag drops events for a slow connection | Medium | Low | Stage 4 never emits events. Stage 7+ is where this matters; `broadcast::channel(64)` is the v4-proven size. `RecvError::Lagged` is treated as `continue` (v4 pattern); events lost to lag are surfaced on the next event. If a client can't keep up, dropping old events is the right call. |
| Accept loop blocks tokio executor on a malicious 1-MiB payload | Low | Low | Deferred from Stage 3 §Architect findings; `serde_json` has `RECURSION_LIMIT` and will fail cleanly. If profiling later shows the parse is a real stall, `tokio::task::block_in_place` at the decode site is a one-line fix. |
| User kills daemon with SIGKILL; pid/socket/version files linger | Medium | Low | `ensure_daemon_if_needed` sees stale pid (via `is_daemon_alive` returning false), calls `sentinel::clean(target)`, and re-forks. The lingering files are auto-cleaned on next client invocation; no manual intervention required. |
| PID reuse: daemon died; OS reassigned the PID to an unrelated process; `is_daemon_alive(pid)` returns true; `kill(pid, SIGTERM)` targets the wrong process | Low | Medium | Stage 4 mitigation: `is_daemon_alive` reads `/proc/<pid>/comm` on Linux and checks the process name is `loopr`; on macOS, uses `ps -p <pid> -o comm=` with the same check. If the name check fails, treat as stale (not alive) and clean up. Imperfect (any binary named `loopr` would collide) but closes the common case. |
| `tokio::runtime::Runtime` drop aborts spawned handler tasks mid-response-send | Low | Low | Stage 4 handlers return quickly (handshake, status both microseconds); a task being aborted mid-send means the client sees connection close after its request — it retries. Stage 7+ may need `JoinSet` + explicit graceful shutdown with timeout; Stage 4 doesn't. |
| `loopr daemon start --foreground` invoked while a background daemon already runs: socket bind fails (address in use) | Medium | Low | Check `.loopr/daemon.pid` before binding; if a live daemon of any mode is present, error out with "daemon already running at pid X; use `loopr daemon stop` first" instead of letting the `UnixListener::bind` error surface raw. |
| `.loopr/daemon.run-id` exists but `runs/<run-id>/` was deleted by hand (or `rkvr`) | Low | Low | `loopr logs` / test tooling that reads `.loopr/daemon.run-id` handles missing run-dir as "daemon-run-id file stale" and falls back to "most recent run dir" (Stage 2 query semantics). Not a daemon-side failure. |
| Two `loopr daemon start` invocations race for the pid file | Low | Low | `OpenOptions::new().create_new(true)` on the pid file means exactly one wins. The loser's grandchild returns `LookprError::LockLost` from `daemon_main`'s lock-acquire phase BEFORE any other side-effects (no version file, no run-id file, no socket bind), and `run_grandchild` maps that to a silent `process::exit(0)` that DOES NOT run `sentinel::clean(target)`. The winner's files stay intact. |
| Stale `.loopr/socket` without a stale PID file causes `EADDRINUSE` on bind | Low | Medium | `daemon_main`'s active-daemon phase unconditionally `remove_file(&socket)` before `UnixListener::bind`. The PID lock we hold at that point is the authority; a lingering socket file on disk is just a side-effect of a previous ungraceful exit, safe to remove. |
| `LOOPR_PROTOCOL_VERSION_OVERRIDE` test env var leaks into a production shell | Low | Low | Gated behind `cfg!(test)` / `cfg!(debug_assertions)`. Release-build binaries ignore it. Documented as test-only in-line. |
| Client polls `connect_or_wait` too aggressively and overwhelms the starting daemon | Low | Low | `POLL_INTERVAL_MS = 100ms` (v4 value), bounded by `START_TIMEOUT_SECS = 3`. 30 attempts max; each is a single `UnixStream::connect` syscall. Negligible load. |

## Open Questions

- [ ] **Per-target or per-user daemon?** `.loopr/` is per-target (vision §Target Repo Layout pins this), so every target gets its own daemon. A user with 5 active targets runs 5 daemons. This is what v3/v4 did; it's cheap (idle daemon is ~5MB RSS). If it ever becomes a cost problem, a per-user daemon with per-target session state is a future stage. Not a Stage 4 decision.
- [ ] **Should `loopr plan "x"` auto-fork the daemon, or error if no daemon?** Current draft auto-forks (best UX). Alternative: require explicit `loopr daemon start` first, error with "no daemon" otherwise. Auto-fork matches v4 and is how users expect CLI tools to behave; keeping it.
- [ ] **`MethodName::PlanCreate` in ipc or use `request_raw` for Stage 4?** Current draft uses raw `&str` "plan.create" in the client because adding the variant to `ipc::MethodName` without adding a handler arm to `dispatch` would be a compile error (the exhaustive match forces the pair). Deferring the `MethodName` variant to Stage 5 keeps Stage 4's blast radius in `loopr`; Stage 5 adds the pair together. The raw-string path is explicitly the "not yet promoted to typed" escape hatch.
- [ ] **Stop escalation: 3 seconds before SIGKILL enough?** v4 uses 10 seconds for `GRACEFUL_SHUTDOWN_SECS`. v5's Stage 4 daemon has essentially no state to flush (no in-flight agents, no taskstore writes). 3 seconds covers telemetry flush + socket cleanup. Revisit once Stage 7+ daemon actually has work to finish.
- [ ] **PID file format: just the PID, or JSON with start-time and version?** v4 wrote the PID as plain text and the version to a sibling file. That's boring and works. The alternative (`{"pid": 12345, "version": "...", "started-at": "..."}`) would consolidate into one atomic file read. Current draft: keep v4's two-file format. Reconsider if the two-file read ever races.
- [ ] **Signal handling on Windows?** Out of scope. v5 is Linux/macOS only (Linux-primary). If Windows ever matters, add a `cfg(windows)` path; the protocol already works over `NamedPipe`s.
- [ ] **`daemon status` output format: human vs. JSON?** Current draft is human-readable multi-line. `--json` flag would emit `StatusResult` directly. Not a Stage 4 concern; add when the first user pipes it to `jq`.
- [ ] **`crates/ipc/CLAUDE.md` drift.** That file still says "Message enums: `Request`, `Response`, `Event` with `#[serde(tag = \"kind\")]` or equivalent tagging", but Stage 3's shipped design chose field-presence discrimination instead (no `kind` tag). Pre-existing drift from Stage 3, not Stage 4 code — flagging here because Stage 4's docs sit next to it and readers cross-referencing might get confused. Fix in a follow-up CLAUDE-md cleanup commit.

## Architect Design-Review Findings (acted on)

Design review surfaced five findings. All five were acted on in this draft.

### Acted: dual fork-path contradiction (the Hardest Question)

- **Concern:** `lib::run` synchronously calls `ensure_daemon_if_needed` BEFORE the parent creates a Tokio runtime or installs telemetry (the safe path), but the async `connect_or_fork` ALSO contained a fallback `daemon::ensure_daemon(target)` call inside the multithreaded Tokio block. If that fallback ever fired: (a) `libc::fork` in a multithreaded process is POSIX-undefined (grandchild deadlock risk), and (b) the grandchild's `telemetry::init` would fail with `AlreadyInitialized` because the parent's `set_global_default` had already flipped the COW-inherited flag.
- **Action:** renamed `connect_or_fork` → `connect_or_wait` and stripped the fallback. The function now only polls for the socket; it never forks. `lib::run`'s synchronous `ensure_daemon_if_needed` is the sole fork authority in the entire codebase.

### Acted: `accept_loop` missing the shutdown-wakeup belt-and-suspenders

- **Concern:** `handle_client` had a loop-top `ctx.shutting_down.load()` atomic check to cover the non-sticky-`notify_waiters` race. `accept_loop` did not. If SIGTERM fired during the microseconds `accept_loop` spends between select-return and spawning the connection task, the notify could be lost and the loop would block on `listener.accept()` forever.
- **Action:** mirrored the atomic check at `accept_loop`'s loop top. Symmetric with `handle_client` now.

### Acted: lock-loser grandchild deletes the winner's sentinel files

- **Concern:** Two concurrent `loopr plan "x"` invocations each fork a grandchild. The losing grandchild's `write_pid` with `create_new(true)` fails cleanly — but the loser would then fall through to the standard `daemon_main` shutdown path, which calls `sentinel::clean(target)` and removes the winner's pid / socket / version / run-id files.
- **Action:** split `daemon_main` into a lock-acquire phase (pid → version → run-id, in that order, NO cleanup on failure) and an active-daemon phase (telemetry init, socket bind, accept loop, WITH cleanup on exit). Lock-acquire failure returns `LooprError::LockLost`; `run_grandchild` catches that variant, silently `process::exit(0)`, and does NOT run `sentinel::clean`.

### Acted: `UnixListener::bind` fails on stale socket if PID file was hand-removed

- **Concern:** If a user hand-deletes `.loopr/daemon.pid` but leaves `.loopr/socket` on disk (or a SIGKILL'd daemon left the socket behind and something else cleared the pid file), `is_daemon_alive` reports no daemon and skips `sentinel::clean`. The grandchild then calls `UnixListener::bind` on the existing socket and fails with `EADDRINUSE`.
- **Action:** in the active-daemon phase, unconditionally `remove_file(&socket)` before `UnixListener::bind`. The PID lock acquired in the previous phase is the real authority; the socket file is just a side-effect.

### Acted: newline-safety in JSON payloads

- **Concern:** a literal `0x0A` byte inside a string field would split the NDJSON line prematurely under `LinesCodec`, producing a parse error on the receiving end.
- **Action:** reclassified from "assumption" to "verified": `serde_json::to_string` ASCII-escapes every control character in string values per RFC 8259, so `\n` in source data becomes `\\n` on the wire. Documented in §Security with a Phase 4 unit test (`{"message": "line1\nline2"}` round-trip) to pin the guarantee.

## References

- [`docs/vision.md`](../../../../docs/vision.md) §ipc (lines 179-196), §loopr (lines 191-198), §Target Repo Layout (lines 270-300), §Worktree crash recovery (line 303; informs the general "reconcile on startup" ethos).
- [`docs/roadmap.md`](../../../../docs/roadmap.md) §Stage 4.
- [`crates/ipc/docs/design/2026-04-19-protocol.md`](../../../ipc/docs/design/2026-04-19-protocol.md) — Stage 3's shipped protocol; the `Method` exhaustiveness contract, handshake ordering, parse-error closing contract are all codified there and enforced here.
- [`crates/loopr/CLAUDE.md`](../../CLAUDE.md) §In scope (IPC transport is in this crate).
- [`docs/design/2026-04-19-telemetry-stage-2.md`](../../../../docs/design/2026-04-19-telemetry-stage-2.md) — format precedent for consolidating roadmap-listed two-doc stages into one.
- v4 reference: `~/repos/scottidler/loopr-v4/src/daemon.rs` — double-fork lifecycle, pid/version files, silent-restart on version drift.
- v4 reference: `~/repos/scottidler/loopr-v4/src/ipc/server.rs` — `IpcServer::{new,bind,cleanup}` + `handle_client` + broadcast-event forwarding. Directly inherited (renamed/reorganized into `transport/`).
- v4 reference: `~/repos/scottidler/loopr-v4/src/ipc/client.rs` — `IpcClient::{connect,request,send,recv,handshake}` + `ClientError`. Directly inherited, re-typed against `ipc::MethodName`.
- v4 reference: `~/repos/scottidler/loopr-v4/src/ipc/codec.rs` — `LinesCodec::new_with_max_length(1 MiB)`. Inherited as `ipc::MAX_LINE_BYTES` (already a Stage 3 const) consumed here.
