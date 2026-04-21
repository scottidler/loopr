//! `Work` record type and `WorkStatus` state machine.
//!
//! A `Work` is a leaf-level implementation unit decomposed from a
//! `Plan`. Stage 6 introduces the type; Stage 7's reactive coordinator
//! drives the FSM through Pending -> Ready -> InProgress -> InReview
//! -> Integrated -> Done as deps clear, agents run, and Bundles merge.
//!
//! Display output on `WorkStatus` is lowercase to match the serde wire
//! form — the `Record` derive calls `ToString::to_string` on indexed
//! fields, so the index map value and the on-disk JSON value must use
//! the same spelling.
//!
//! The Phase 2 scope of this file is the `WorkStatus` enum only; the
//! `Work` struct and its constructor/transition methods land in Phase
//! 3 of hierarchy.md.

use serde::{Deserialize, Serialize};
use strum::Display;

use derive::Fsm;

/// Lifecycle state for `Work`. Ports v4's 10-state `work.yml` table,
/// with round-2 Architect adjustments applied: `Integrated =>
/// Superseded` added so a superseded parent `Plan` can cascade-cancel
/// Works mid-integration; `Ready => Done` demoted from the routine
/// transitions table to `overrides(...)` so the no-op-Work bypass
/// cannot be used as an AC-skipping loophole on the normal path;
/// `Integrated => Done` is `Coordinator`-only because the "no active
/// sessions" guard needs daemon-level visibility the `integrator`
/// crate does not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Display, Fsm)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[fsm(
    role = crate::Role,
    terminal = [Done, Superseded, Abandoned],
    transitions(
        Draft       => Pending    by (Coordinator),
        Draft       => Ready      by (Coordinator),
        Draft       => Superseded by (Coordinator, Director),
        Draft       => Abandoned  by (Coordinator, Director),
        Pending     => Ready      by (Coordinator),
        Pending     => Superseded by (Coordinator, Director),
        Pending     => Abandoned  by (Coordinator, Director),
        Ready       => InProgress by (Coordinator),
        Ready       => Blocked    by (Coordinator),
        Ready       => Superseded by (Coordinator, Director),
        Ready       => Abandoned  by (Coordinator, Director),
        InProgress  => Blocked    by (Coordinator, Implementer),
        InProgress  => InReview   by (Implementer),
        InProgress  => Superseded by (Coordinator, Director),
        InProgress  => Abandoned  by (Coordinator, Director),
        Blocked     => Ready      by (Coordinator),
        Blocked     => Superseded by (Coordinator, Director),
        Blocked     => Abandoned  by (Coordinator, Director),
        InReview    => InProgress by (Coordinator),
        InReview    => Integrated by (Integrator),
        InReview    => Superseded by (Coordinator, Director),
        InReview    => Abandoned  by (Coordinator, Director),
        Integrated  => Done       by (Coordinator),
        Integrated  => Superseded by (Coordinator, Director),
        Integrated  => Abandoned  by (Coordinator, Director),
    ),
    overrides(
        Ready      => Done     by (Coordinator),
        InProgress => Ready    by (Coordinator),
        InProgress => InReview by (Coordinator),
        InReview   => Ready    by (Coordinator),
    ),
)]
pub enum WorkStatus {
    Draft,
    Pending,
    Ready,
    InProgress,
    Blocked,
    InReview,
    Integrated,
    Done,
    Superseded,
    Abandoned,
}
