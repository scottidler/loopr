//! Typed control frames for the `events.subscribe` stream (Phase 17 of
//! `docs/design/2026-07-11-verified-swarm.md`).
//!
//! An `events.subscribe` connection carries three kinds of wire frame, all
//! serialized as the existing [`DaemonEvent`] `{event, data}` envelope so
//! the client's decode path is unchanged:
//!
//! - a real daemon lifecycle event ([`WatchFrame::Event`]),
//! - a periodic keepalive ([`WatchFrame::Heartbeat`]), and
//! - a lag/gap marker ([`WatchFrame::Gap`]) emitted when the daemon's
//!   broadcast channel dropped events before this slow client could read
//!   them.
//!
//! The reserved `event` names below are the wire discriminators; the
//! [`WatchFrame`] enum is the TYPED surface both sides match on, so the
//! gap marker is a typed variant carrying a `dropped` count rather than a
//! magic string parsed at the call site.

use serde::{Deserialize, Serialize};

use crate::envelope::DaemonEvent;

/// Reserved `DaemonEvent::event` name for the periodic keepalive the
/// daemon sends on an `events.subscribe` stream. A real daemon event
/// never uses this name (daemon events are `work.*` / `plan.* / `budget.*`).
pub const STREAM_HEARTBEAT_EVENT: &str = "stream.heartbeat";

/// Reserved `DaemonEvent::event` name for the broadcast-lag gap marker.
/// The `data` object carries `{ "dropped": <u64> }` — the number of events
/// the daemon's broadcast channel discarded before this client caught up.
pub const STREAM_GAP_EVENT: &str = "stream.gap";

/// A typed classification of one frame received on an `events.subscribe`
/// stream. Both the daemon (constructing control frames) and the client
/// (rendering) match on this enum rather than on the raw wire `event`
/// string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WatchFrame {
    /// A real daemon lifecycle event (`work.*` / `plan.*` / `budget.*`).
    Event(DaemonEvent),
    /// Server keepalive. Carries no payload; the client renders nothing,
    /// but its arrival (and the successful write on the daemon side) is
    /// proof the stream is alive during a quiet period.
    Heartbeat,
    /// The daemon's broadcast channel lagged and dropped `dropped` events
    /// before this client could read them. Surfaced to the operator as a
    /// VISIBLE discontinuity marker — never silently swallowed.
    Gap { dropped: u64 },
}

impl WatchFrame {
    /// Build the heartbeat wire frame the daemon writes each interval.
    pub fn heartbeat_event() -> DaemonEvent {
        DaemonEvent {
            event: STREAM_HEARTBEAT_EVENT.to_string(),
            data: serde_json::Value::Null,
        }
    }

    /// Build the gap wire frame the daemon writes on `RecvError::Lagged`.
    pub fn gap_event(dropped: u64) -> DaemonEvent {
        DaemonEvent {
            event: STREAM_GAP_EVENT.to_string(),
            data: serde_json::json!({ "dropped": dropped }),
        }
    }

    /// Classify a `DaemonEvent` read off an `events.subscribe` stream into
    /// its typed [`WatchFrame`]. The two reserved control-frame names map
    /// to `Heartbeat` / `Gap`; everything else is a real `Event`.
    pub fn classify(event: DaemonEvent) -> WatchFrame {
        match event.event.as_str() {
            STREAM_HEARTBEAT_EVENT => WatchFrame::Heartbeat,
            STREAM_GAP_EVENT => {
                // Defensive: a malformed gap frame with no/bad `dropped`
                // still renders as a gap (dropped=0) rather than being
                // misread as a real event.
                let dropped = event
                    .data
                    .get("dropped")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                WatchFrame::Gap { dropped }
            }
            _ => WatchFrame::Event(event),
        }
    }
}
