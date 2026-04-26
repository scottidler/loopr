//! Per-process and per-session digests.
//!
//! Phase 7 of the Tier-1 cleanup wires a `ProcessSnapshot` counter
//! struct that the daemon updates during its lifetime, with a
//! per-process digest written at graceful and abnormal exit. Phase 8
//! wires the per-session digest aggregation that walks every
//! per-process digest under a session and rolls them up.
//!
//! Layout under `$XDG_DATA_HOME/loopr/`:
//!
//! - `sessions/<sid>/targets/<slug>/runs/<pid>/summary.md` — per-process
//! - `sessions/<sid>/summary.md`                          — per-session

pub mod cost;
pub mod process;
pub mod session;
