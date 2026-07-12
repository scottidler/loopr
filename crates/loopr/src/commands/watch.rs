//! `loopr watch [--plan <id>]` body (Phase 17 of
//! `docs/design/2026-07-11-verified-swarm.md`).
//!
//! Opens a long-lived `events.subscribe` stream to the daemon and renders
//! the live `DaemonEvent` feed as ONE LINE per event until the daemon shuts
//! down or the operator interrupts with ctrl-c. Optional `--plan` filters
//! the stream to a single Plan; events without a Plan scope (process-wide
//! budget events) always pass through.
//!
//! Like every other read verb (Phase 16), `watch` does NOT auto-fork a
//! daemon: with none running it prints "no daemon running" and exits.

use std::ops::ControlFlow;
use std::path::Path;

use ipc::WatchFrame;

use crate::error::LooprError;
use crate::transport::{self, ClientTimeouts};

/// `loopr watch` entry point.
#[tracing::instrument(
    name = "client.watch",
    level = "info",
    skip_all,
    fields(target = %target.display(), plan = plan_filter.as_deref().unwrap_or("*"), subcommand = "watch"),
    err,
)]
pub fn run(target: &Path, plan_filter: Option<String>) -> Result<(), LooprError> {
    if !crate::daemon::is_running(target)? {
        println!("no daemon running");
        return Ok(());
    }

    // Honor the target's `transport:` config for connect budgets, matching
    // `transport::ipc_call`. The request wall-clock cap is irrelevant here:
    // a subscription is long-lived and never routed through `request_impl`.
    let timeouts = match crate::config::Config::load(target) {
        Ok(cfg) => ClientTimeouts::from(&cfg.transport),
        Err(e) => {
            tracing::warn!(error = %e, "config load failed; using default client timeouts");
            ClientTimeouts::default()
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait_with_timeouts(target, timeouts).await?;
        client.handshake(None).await?;

        let plan = plan_filter.as_deref();
        // Race the long-lived stream against ctrl-c so the operator can
        // detach cleanly. Dropping `client` on either exit closes the
        // socket, which the daemon observes to tear down its server-side
        // subscription (no leaked forwarder).
        let outcome = tokio::select! {
            res = client.subscribe_events(|frame| {
                if let Some(line) = render_watch_line(&frame, plan) {
                    println!("{line}");
                }
                ControlFlow::Continue(())
            }) => res,
            _ = tokio::signal::ctrl_c() => Ok(()),
        };
        outcome
    })
}

/// Render one classified [`WatchFrame`] into a single output line, or
/// `None` when the frame produces no output (a heartbeat, or an event
/// filtered out by `--plan`).
///
/// Pure and total so it is unit-testable without a live daemon:
/// - `Heartbeat` -> `None` (silent liveness keepalive).
/// - `Gap { dropped }` -> a VISIBLE discontinuity marker (never filtered:
///   a gap is a stream-wide fact, not a per-Plan one).
/// - `Event` -> `"<event>  <compact-json-data>"`, filtered by `plan` when a
///   filter is set AND the event carries a `plan_id` that does not match.
///   Events with no `plan_id` (process-wide budget events) always render.
pub(crate) fn render_watch_line(frame: &WatchFrame, plan_filter: Option<&str>) -> Option<String> {
    match frame {
        WatchFrame::Heartbeat => None,
        WatchFrame::Gap { dropped } => Some(format!(
            "--- gap: {dropped} event(s) dropped (consumer fell behind) ---"
        )),
        WatchFrame::Event(ev) => {
            if let Some(want) = plan_filter
                && let Some(pid) = ev.data.get("plan_id").and_then(serde_json::Value::as_str)
                && pid != want
            {
                return None;
            }
            let data = serde_json::to_string(&ev.data).unwrap_or_else(|_| "{}".to_string());
            Some(format!("{}  {}", ev.event, data))
        }
    }
}

#[cfg(test)]
mod tests;
