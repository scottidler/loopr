# Design Document: IPC and Daemon-Startup Timeouts

**Author:** Scott Idler
**Date:** 2026-05-09
**Status:** Implemented
**Crates touched:** loopr, ipc
**Review Passes Completed:** 4/5 (Architect Round 2 fixes incorporated)

## Summary

Today the daemon-client IPC has no per-request timeout, no server-side idle timeout, no server-side write timeout, and no daemon-startup watchdog. A dead daemon, a stuck `handle_client`, a SIGSTOPped client whose socket buffer fills up, or a hung `build_context` all manifest as a process that waits forever. This doc adds four bounded timeouts with sane const defaults at the callsites and a single `transport` section in `.loopr/config.yml` that lets an operator override each value.

## Problem Statement

### Background

A `loopr show xx-abcde` invocation in the field hung indefinitely. Investigation showed `request_impl` (`crates/loopr/src/transport/client.rs:102`) has a 3-second connect deadline (via `connect_or_wait`), but once the framed sink/stream is up, the response loop blocks on `framed.next().await` forever. The Architect (Gemini) reviewed the surface across two rounds and flagged four real gaps (and one non-issue):

1. **Client request timeout** missing.
2. **Server-side idle timeout** missing on `handle_client` (`crates/loopr/src/transport/server.rs:149`). A SIGSTOPped or deadlocked client that holds its socket open without sending or receiving keeps a `JoinSet` slot alive forever.
3. **Daemon startup watchdog** missing. `build_context` (`crates/loopr/src/daemon.rs:470`) calls `store::Store::open` and `startup::reconcile` with no time bound; a corrupted JSONL or stuck worktree sweep orphans the grandchild before it ever binds the socket.
4. **Server-side write timeout** missing. The broadcast-event send path (`crates/loopr/src/transport/server.rs:233`) and the response-write path both await `framed.send(...).await` unbounded. A SIGSTOPped client whose unix-domain socket send buffer fills causes the daemon's `handle_client` task to block in `send` indefinitely — a read-side idle timer never fires because the task is no longer in the `select!`. (Architect Round 2 finding; correctly identified as the second hand of the zombie-connection problem from #2.)

(The Architect also flagged "clean client disconnect" in Round 1 — already correct: when a client closes its end, `framed.next()` returns `None` and the handler exits. No fix needed.)

### Problem

Four distinct hangs, all unbounded today:

- **Client → daemon:** `IpcClient::request_impl`'s response loop has no timeout. Symptom: CLI hangs.
- **Daemon → client (read side):** `handle_client`'s `framed.next()` has no idle timeout. Symptom: zombie connection slot when the client never sends another byte.
- **Daemon → client (write side):** `handle_client`'s `framed.send` (both response and event-broadcast paths) has no write timeout. Symptom: zombie connection slot when the client's read side stalls and the kernel send buffer fills — orthogonal to the read-side idle timer.
- **Grandchild startup:** `build_context` has no watchdog. Symptom: orphaned `loopr daemon start` process with no log line and no socket.

### Goals

1. Every wait that can hang on a peer or on disk is wrapped in `tokio::time::timeout` with a documented budget.
2. Defaults are encoded as `pub const` next to the existing drain-timeout consts in `crates/loopr/src/daemon.rs` (or in the transport modules where they fit better).
3. Operators can override each default in `.loopr/config.yml` under a `transport:` section, matching the shape of `IntegratorSection`.
4. Adding a new IPC method is a single-line change at the callsite — pass the right `Duration`, no dispatch table to maintain elsewhere.

### Non-Goals

- Per-method timeout classes (fast vs slow). Today every `MethodName` returns synchronously after a store read/write or a spawn trigger. No method blocks on the LLM. We design `request_impl` to take a `Duration` parameter so a future slow method can pass a longer value, but we do not pre-create a `client-slow-secs` config field for a method that does not exist.
- Reconnect-on-timeout semantics in `IpcClient`. Short-lived clients drop after one round trip; the caller can re-run the command.
- Cancellation propagation back to the daemon when a client times out. The daemon-side request keeps running and writes its result; the client just stops waiting. This matches the existing async-ack model for `plan.create`.
- Timeouts on `serve` / `accept_loop` themselves. Those are intentionally unbounded — they live for the daemon's lifetime.
- **Decoupling `handler::dispatch` from the `event_rx` arm.** The Architect (Round 2) correctly observed that `handle_client` awaits `dispatch(...).await` inline inside the `select!` body, which means a slow handler pauses event delivery on this connection — fast enough on the broadcast channel that under load it can yield `RecvError::Lagged` and silently drop events. This is an existing property of `handle_client`, not something this doc introduces. Today every method dispatches in well under a second, so the symptom does not appear; when a slow synchronous method ships, the fix is to spawn `dispatch` into a per-request task and join via `JoinSet`. That is its own design doc.
- **Idle-on-wire (per-message) client request timeout.** Today's `request_impl` uses a single wall-clock cap from start of request to receipt of matching response. The Architect (Round 2) noted this would violently kill a future streaming method that sends progress events for >`client_request_secs` before its terminal response. No such method exists today; when one ships, upgrade `request_impl` to reset its timer on each `IpcMessage` received from the framed stream.

## Proposed Solution

### Overview

Four bounded timeouts:

| Timeout | Where it wraps | Default | Rationale |
|---|---|---|---|
| Client request | response loop in `IpcClient::request_impl` | 10 s | Today's methods are all sub-second; 10 s is a 10-100x ceiling that catches dead-daemon hangs without false-firing on a healthy daemon under load |
| Server idle | pinned `Sleep` watching read silence in `handle_client` | 15 s | Slightly larger than the client request timeout so a healthy round trip never trips the server side first. Survives `select!` iterations (see Architecture below) |
| Server write | both `framed.send` calls in `handle_client` (response and broadcast-event) | 10 s | Catches a SIGSTOPped/stalled-reader client whose unix-domain send buffer fills. Smaller than the read-side idle so a stuck-reader client is dropped on the next event broadcast rather than waiting for the read-idle window to elapse |
| Daemon startup | call to `build_context` in `run_active_daemon` | 60 s | Store open + worktree reconcile + `.git/info/exclude` install, on a target with hundreds of pending worktrees, can legitimately take tens of seconds; 60 s catches genuine hangs without false-firing on a slow but-working filesystem |

### Architecture

Four separate changes, one per timeout:

1. **`IpcClient::request_impl` gains a `timeout: Duration` parameter.** Every callsite passes a `const Duration` owned by that callsite's module. `IpcClient::handshake`, `IpcClient::request`, and `IpcClient::request_raw` each pick their own const.
2. **`handle_client` runs a pinned `tokio::time::Sleep` outside the `select!` loop and adds it as a fourth `select!` arm.** Naively wrapping the `framed.next()` arm in `tokio::time::timeout(...)` is wrong: every iteration of the `select!` constructs a fresh timeout future, and any `event_rx.recv()` firing causes the loop to iterate and reset the timer to zero — so a healthy daemon broadcasting events would prevent the read-idle timer from ever expiring (Architect Round 2 finding). The correct shape is a `Pin<Box<Sleep>>` whose deadline is set once and only `reset()` when the `framed.next()` arm produces a value (real client traffic). Concrete shape:
   ```rust
   let mut idle = Box::pin(tokio::time::sleep(server_idle));
   loop {
       tokio::select! {
           biased;
           _ = ctx.shutdown_notify.notified() => return,
           _ = &mut idle => {
               warn!("server idle timeout exceeded; closing connection");
               return;
           }
           res = framed.next() => match res {
               Some(Ok(line)) => {
                   idle.as_mut().reset(tokio::time::Instant::now() + server_idle);
                   /* existing dispatch */
               }
               Some(Err(e)) => { /* existing codec-error branch */ }
               None => return,
           },
           event = event_rx.recv() => match event { /* existing — note: idle timer NOT reset on broadcasts */ }
       }
   }
   ```
3. **Both `framed.send(...).await` calls in `handle_client` are wrapped in `tokio::time::timeout(server_write, ...)`.** This addresses the Architect's Round 2 zombie-client-with-full-send-buffer scenario: a SIGSTOPped client's kernel send buffer fills, the daemon attempts to push a broadcast event, and the unbounded `send` would block the whole task forever — outside the `select!`, so the read-side idle timer never gets the chance to fire. On `Err(_)` from the timeout, log a `warn!` and `return` (close the connection). Both the response-write at `server.rs:184` and the event-broadcast-write at `server.rs:233` get the same treatment.
4. **`run_active_daemon` wraps the call to `build_context` in `tokio::time::timeout`.** The Architect's Round 1 literal phrasing was "wrap `run_active_daemon` in `daemon_main`" — that's wrong, because `run_active_daemon` includes `serve`, the unbounded accept loop. The correct scope is `build_context`, which is the only async work that can hang on disk/state. (Confirmed in the Round 1 dialogue.)

A new `TransportSection` in `crates/loopr/src/config.rs` exposes the four durations; `Config::load` populates it; consumers read from `ctx.config.transport.*` (or hold a typed copy on the relevant struct, mirroring how `IntegratorConfig` flows through `build_context`).

### Data Model

```rust
// crates/loopr/src/config.rs

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct TransportSection {
    /// Wall-clock cap on the response wait inside `IpcClient::request_impl`.
    /// Bounds dead-daemon hangs and protocol-mismatch waits. Default: 10s.
    pub client_request_secs: u64,

    /// Wall-clock cap on read silence inside `handle_client`. The pinned
    /// `Sleep` driving this is reset only when `framed.next()` yields real
    /// client traffic; broadcast events do NOT reset it. A peer that
    /// holds the socket open without sending for this long is dropped.
    /// Default: 15s.
    pub server_idle_secs: u64,

    /// Wall-clock cap on each `framed.send(...).await` inside
    /// `handle_client` (both the response-write and event-broadcast-write
    /// paths). Bounds the SIGSTOPped-client / full-send-buffer hang.
    /// Default: 10s.
    pub server_write_secs: u64,

    /// Wall-clock cap on `build_context` (Store::open + startup::reconcile +
    /// excludes install). Beyond this, the grandchild exits with
    /// `LooprError::DaemonStartup` rather than orphaning. Default: 60s.
    pub daemon_startup_secs: u64,
}

impl Default for TransportSection {
    fn default() -> Self {
        Self {
            client_request_secs: 10,
            server_idle_secs: 15,
            server_write_secs: 10,
            daemon_startup_secs: 60,
        }
    }
}
```

`Config` gains `pub transport: TransportSection` next to `integrator`.

### API Design

`request_impl` signature change:

```rust
async fn request_impl(
    &mut self,
    method: &str,
    params: serde_json::Value,
    timeout: Duration,                                     // NEW
) -> Result<(DaemonResponse, Vec<DaemonEvent>), LooprError>
```

The response loop body is wrapped in `tokio::time::timeout(timeout, ...)`. On expiry, return `LooprError::ClientIo(format!("request timed out after {:?}", timeout))`.

The three public callers each acquire a `Duration` from a const owned at their callsite:

- `IpcClient::handshake` uses `CLIENT_HANDSHAKE_TIMEOUT` (defaults to client_request_secs from config; in practice the client builds an `IpcClient` from a `Config` it already has, so the timeouts are passed in via constructor or per-call argument; see "Implementation Plan" for the wiring choice).
- `IpcClient::request` uses `CLIENT_REQUEST_TIMEOUT`.
- `IpcClient::request_raw` uses the same.

Wiring choice: `IpcClient::connect` gains a `timeouts: ClientTimeouts` parameter (a small struct holding the three durations) so the framed connection carries its budgets with it, and `request_impl` reads from that struct. Default-constructed via `ClientTimeouts::from(&TransportSection)`. CLI bodies (`commands/show.rs`, `commands/list.rs`, etc.) load `Config`, build `ClientTimeouts`, and hand it to `connect_or_wait` (which forwards to `IpcClient::connect`).

Server side, `handle_client` reads from `ctx.transport_timeouts.server_idle` (a typed copy on `DaemonContext`).

Daemon side, `run_active_daemon` reads from `config.transport.daemon_startup_secs` directly — `build_context` is what we're bounding, so the timeout has to live in the caller.

### Implementation Plan

#### Phase 1: Config plumbing

**Model:** sonnet

- Add `TransportSection` to `crates/loopr/src/config.rs` with the three `*_secs: u64` fields and `Default` matching the table above.
- Add `pub transport: TransportSection` to `Config`.
- Add a `ClientTimeouts` struct (in `crates/loopr/src/transport.rs` — same file as `connect_or_wait`) holding three `Duration`s; `From<&TransportSection> for ClientTimeouts`.
- Add round-trip serde test (load YAML with `transport: { client-request-secs: 5, ... }`, serialize back, compare).
- Verify with `otto check -p loopr`.

#### Phase 2: Daemon startup watchdog

**Model:** sonnet

- Add `pub const DAEMON_STARTUP_TIMEOUT_DEFAULT_SECS: u64 = 60;` next to the other `*_TIMEOUT_SECS` consts in `crates/loopr/src/daemon.rs`.
- In `run_active_daemon` (daemon.rs:432), wrap the `build_context(...).await` call in `tokio::time::timeout(Duration::from_secs(config.transport.daemon_startup_secs), ...)`.
- On `Err(_)` (the elapsed branch), return `LooprError::DaemonStartup(format!("build_context exceeded {N}s startup budget"))`. No new error variant needed.
- Test: a unit test that calls `build_context` with a stub `Store` whose `open` future never completes, wrapped in a 100 ms timeout, and asserts the right `DaemonStartup` error string.

#### Phase 3: Server idle timeout (read side)

**Model:** sonnet

- Add a `ServerTimeouts` field to `DaemonContext` populated from `config.transport` at `build_context` time. Holds `idle: Duration` and `write: Duration`.
- In `handle_client`, hoist a pinned `Sleep` outside the `loop` and add it as a fourth `select!` arm. The shape is the one in the Architecture section above; the key invariants are (a) the `Sleep` is pinned outside the loop so it survives `select!` iterations, (b) only the `framed.next()` arm calls `idle.as_mut().reset(Instant::now() + ...)`, and (c) the `event_rx.recv()` arm explicitly does NOT reset the idle timer (otherwise a chatty broadcast keeps a silent client alive forever — Architect Round 2 finding).
- Integration test: open a `UnixStream` to the daemon socket, complete the handshake, then sit silent for `server_idle + buffer`. Assert the daemon closes the connection. Second variant: while sitting silent, have the daemon broadcast events on `event_rx` faster than `server_idle` and assert the connection still closes after the first `server_idle` window past the last client byte.

#### Phase 3b: Server write timeout

**Model:** sonnet

- Wrap the response-write `framed.send(line_out).await` (currently `server.rs:184`) in `tokio::time::timeout(server_write, ...)`. On elapsed: `warn!("server write timeout exceeded (response); closing connection")` and `return`.
- Wrap the broadcast-event `framed.send(line_out).await` (currently `server.rs:233`) in the same way. On elapsed: `warn!("server write timeout exceeded (event); closing connection")` and `return`.
- Both branches treat elapsed identically to the existing `is_err()` paths (close the connection). The `JoinSet` slot is freed when the task returns.
- Integration test: open a `UnixStream` to the daemon socket, complete the handshake, then stop reading from the socket without closing it. Have the daemon broadcast events until the client's send buffer fills (a few KB on a unix-domain socket). Assert the connection is closed within `server_write + buffer`. This directly exercises the SIGSTOPped-client scenario without needing a real SIGSTOP.

#### Phase 4: Client request timeout

**Model:** sonnet

- Add the `timeout: Duration` parameter to `request_impl`.
- Wrap the entire body of `request_impl` (the initial `framed.send(line).await` plus the response loop) in a single `tokio::time::timeout(timeout, async { ... })`. Wrapping both ensures the wall-clock cap covers a daemon that accepts the connection but whose read side has hung (kernel send buffer eventually fills; `send` blocks). On elapsed, return `LooprError::ClientIo("request timed out after Ns")`.
- Wire `ClientTimeouts` through `IpcClient::connect` and store it on the struct. `request`, `request_raw`, and `handshake` each pull from the stored timeouts.
- Add `transport::connect_or_wait_with_timeouts(target, &ClientTimeouts)`. Keep `connect_or_wait(target)` as a thin wrapper that calls the new function with `ClientTimeouts::default()`, so existing callsites that don't care about overrides aren't forced to load `Config` on every short-lived CLI invocation.
- For the four current callsites (`commands/show.rs`, `commands/list.rs`, `lib.rs:247`, `lib.rs:285`): leave them on `connect_or_wait` — they pick up the defaults automatically. When operator config tuning becomes necessary, individual commands can opt into `connect_or_wait_with_timeouts(...)` after loading `Config`. (Loading `Config` from `.loopr/config.yml` on every short-lived CLI invocation is one extra fs::read; acceptable but not free, so we don't pay it until a command needs to.)
- Acceptance test (replaces the current hang scenario): start a daemon, kill it without removing the socket file, run `loopr show <id>` against the dead-but-bound socket, assert it returns `LooprError::ClientIo("request timed out after 10s")` within 11 s.

#### Phase 5: Documentation and rollout

**Model:** sonnet

- Add a `transport:` example block to `crates/loopr/CLAUDE.md` and to whatever sample `.loopr/config.yml` ships with the scaffolder (if any).
- Update `crates/loopr/CLAUDE.md`'s "Instrumentation (client side)" / "Instrumentation (daemon side)" sections to mention the new timeout-elapsed log lines.
- Note in `docs/roadmap.md` that the IPC surface now has bounded waits.

## Alternatives Considered

### Alternative 1: Per-method dispatch table (Architect's original suggestion)

- **Description:** Hardcode a `match MethodName { SystemHandshake | SystemStatus | RecordList | RecordGet | PlanCreate => Fast(10s), }` in `request_impl`, with a `Slow(90s)` arm reserved for future LLM-blocking methods.
- **Pros:** Self-documenting — the table says "every method has a class". Explicit forcing function when adding a method (compile error if you forget to assign a class).
- **Cons:** Today every method is Fast, so the table has one column; the Slow arm is dead weight. Adds a layer (`enum RequestKind`) that exists only to be a placeholder. When a slow method finally arrives, we'll know to add the class because the new method's `request_impl` callsite will need a longer timeout — the deferred decision is cheap.
- **Why not chosen:** YAGNI. Per the project memory rule "Options with sane defaults at every turn — typed Enum via config/ENV/CLI with defensible default; don't invent knobs just to have them." Adding the slow lane today, with no method to use it, is inventing a knob.

### Alternative 2: Single timeout, no per-callsite param

- **Description:** One global `request_timeout: Duration` on `IpcClient`. `request_impl` reads from `self.timeout` with no parameter.
- **Pros:** Simpler signature.
- **Cons:** Removes the affordance the user explicitly requested — that each callsite carry its own const default, and that adding a slow method later is one line. With a single struct field, the slow method either has to mutate the field before the call (race-y, easy to forget to reset) or build a second `IpcClient` (heavier).
- **Why not chosen:** The per-callsite `Duration` parameter is the affordance that makes this design extensible without restructuring. The user asked for it explicitly.

### Alternative 3: Architect's literal scope for the daemon watchdog (`run_active_daemon`)

- **Description:** Wrap `run_active_daemon` itself in a 60 s timeout.
- **Pros:** None — it's wrong.
- **Cons:** `run_active_daemon` includes the call to `serve(ctx)`, which is the unbounded accept loop. A 60 s wrap would terminate the daemon 60 seconds after startup completes. The Architect's intent was clearly "bound the prelude before bind"; the literal scope doesn't achieve that.
- **Why not chosen:** Wrong scope. We bound `build_context` instead, which is where the only hangable awaits live.

## Technical Considerations

### Dependencies

No new crates. `tokio::time::timeout` and `serde` are already in the workspace.

### Performance

Adding a `tokio::time::timeout` wrapper costs one heap allocation for the `Sleep` future and a registration with the runtime's timer wheel — measured at hundreds of nanoseconds. Below the noise floor for any IPC call.

### Security

Idle timeout on the server hardens the daemon against a malicious or buggy local client that opens many connections and holds them. The 15 s budget plus a 1 MiB max-line cap on the codec means a single bad actor on the same host can't pin more than a bounded number of `JoinSet` slots. Connection limit cap is out of scope for this doc but worth noting as a follow-up if multiple-client-per-target ever materializes.

### Testing Strategy

Five new tests, each parameterized to use a sub-second timeout so the suite runs fast:

1. **Daemon startup timeout (`daemon::tests`):** stub `Store::open` to never resolve, set `daemon_startup_secs = 0` (or use a `Duration::from_millis(50)` test override on the timeout call); assert that `run_active_daemon` returns `LooprError::DaemonStartup` whose message matches `/exceeded \d+s startup budget/` within wall-clock 1 s.
2. **Server idle timeout — silent client (`transport::server::tests`):** spawn a daemon test harness with `server_idle = Duration::from_millis(100)` on `DaemonContext`'s `ServerTimeouts`, connect, complete handshake, sleep 200 ms, assert the client side reads `None` (clean close) and the daemon's handler task exits.
3. **Server idle timeout — chatty broadcast does not reset timer (`transport::server::tests`):** same setup as #2, but during the silent window have the daemon broadcast events at 50 ms cadence (`< server_idle`). Assert the connection still closes within `server_idle + buffer` past the last client byte. This is the test that catches the Round 2 reset bug — naive `tokio::time::timeout(server_idle, framed.next())` inside `select!` would let this test run forever.
4. **Server write timeout (`transport::server::tests`):** open a `UnixStream` to the daemon, complete handshake, drop the read half (or just stop reading), have the daemon broadcast events until the unix-domain send buffer fills, assert the connection is closed within `server_write + buffer`.
5. **Client request timeout (`transport::client::tests`):** start a sham server that accepts the connection, completes the handshake, then never replies; build the `IpcClient` with `client_request_secs = 0` (override at construction); assert `request_impl` returns `ClientIo("request timed out after ...")` within wall-clock 500 ms.

Each test should configure its budget by constructing the relevant timeouts struct directly with sub-second `Duration`s — not by editing const defaults — so the production constants stay at production values and the test latency stays low. The existing test-only env-var precedent (`LOOPR_PROTOCOL_VERSION_OVERRIDE` in `crates/loopr/src/transport/client.rs:159-162`) is the wrong shape here because timeouts are passed by value, not read from the environment at every check.

Existing tests (handshake round trip, request/response happy paths) all pass without modification because the new defaults are larger than every test's actual round-trip latency.

### Rollout Plan

One commit per phase. Phases 1-4 are individually shippable and don't need a coordinated cutover. Phase 5 is doc-only.

After Phase 4 lands, kill any stale daemon-start zombie processes manually (`pkill -f 'loopr daemon start'`) — this fix prevents future zombies but doesn't reach back to clean existing ones.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Default 10 s client request timeout is too tight on a loaded host | Low | Medium | Operator overrides `transport.client-request-secs` in `.loopr/config.yml`; default is conservative (10-100x typical); we widen if field reports come in |
| Default 60 s startup timeout false-fires on a target with thousands of worktrees | Low | High (daemon won't start) | `startup::reconcile` is `O(worktrees)` but each worktree check is tens of ms; 60 s budgets ~6000 worktrees. If a real deployment exceeds that, operator overrides via config |
| Server-side idle timeout drops a long-blocked client whose request is still being processed by the daemon | Medium | Low | The handler-side dispatch is awaited inline before the next `framed.next()` is awaited — the idle timer only ticks when we're waiting for the client's next line. A slow daemon-side handler does not trip the idle timer |
| `LooprError::DaemonStartup` for the timeout case is indistinguishable from other startup failures in the run log | Low | Low | The error string includes "exceeded Ns startup budget"; grep-match-able. If we ever need machine-distinguishable handling, add a typed variant later |
| Future slow IPC method (e.g. synchronous decompose) is added without bumping the client timeout | Medium | Medium | `request_impl` requires a `Duration` parameter — there is no implicit default. Adding the new callsite forces the developer to pick a value. We add a `client-slow-secs` config field at that point |
| Pinned-`Sleep` reset semantics get inverted later (e.g. someone "fixes" the broadcast arm to also reset idle) | Low | High | The Phase 3 integration test #3 (chatty broadcast must not keep a silent client alive) fails immediately on that mistake. The reset call is a single line; comment it explicitly with the Round 2 finding so a future reader understands why it's narrow |
| Server-write timeout falsely fires under heavy event broadcast on a slow but healthy reader | Low | Medium | 10 s default is multiple orders of magnitude larger than the time it takes to drain a unix-domain send buffer for any realistic event payload (events are JSON lines well under 1 MiB and the buffer is typically 200+ KiB). Operator override available via `transport.server-write-secs` if a deployment ever needs it |

## Open Questions

- [ ] Should the timeout struct be one shared `TransportTimeouts` (used by both client and server) or two separate types (`ClientTimeouts` + `ServerTimeouts`)? The fields don't overlap (client cares about `client_request`; server cares about `server_idle`; `daemon_startup` is consumed only in `run_active_daemon`, neither client nor server). Two narrow types keep `IpcClient` and `DaemonContext` from carrying a field they don't read. Default: two narrow types, both built from the same `TransportSection`.
- [ ] Where should the timeout types live? Strict reading of `crates/ipc/CLAUDE.md` says "no async I/O, no socket lifecycle" — these are transport-side concerns, so they belong in `crates/loopr/src/transport.rs` next to `connect_or_wait`. Revisit if a future TUI crate needs to import them; at that point promote to a shared crate, not to `ipc`.
- [ ] Should the `transport` config section also expose `connect_or_wait`'s polling cadence and ceiling (currently hardcoded in `crates/loopr/src/transport.rs:21-26`)? Out of scope for this doc unless we hit a case where they need tuning.

## References

- `crates/loopr/src/transport/client.rs:102` — `request_impl` (current, no timeout)
- `crates/loopr/src/transport/server.rs:149` — `handle_client` (current, no idle timeout)
- `crates/loopr/src/daemon.rs:392-444` — `run_active_daemon` and the call to `build_context`
- `crates/loopr/src/daemon.rs:470-604` — `build_context` (the function we wrap)
- `crates/loopr/src/config.rs:30-61` — `IntegratorSection` (the shape `TransportSection` mirrors)
- `crates/ipc/src/method.rs:11-22` — `MethodName` (today's five variants, all fast)
- Project memory: `feedback-config-knobs-with-defaults.md` — "Options with sane defaults at every turn"
- Architect consultation transcript: this conversation, prior turns
