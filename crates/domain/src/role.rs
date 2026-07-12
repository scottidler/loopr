use serde::{Deserialize, Serialize};
use strum::Display;

/// `Serialize`/`Deserialize` were added in Phase 7 (verified-swarm) so
/// `CheckRun.executor: Role` persists on-disk. The serde wire form is
/// `kebab-case`, matching the strum `Display` spelling, so the JSON value
/// and any log/index rendering agree (same pattern as `BundleStatus`,
/// whose lowercase serde and lowercase `Display` are kept in lockstep).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Display, Serialize, Deserialize)]
#[strum(serialize_all = "kebab-case")]
#[serde(rename_all = "kebab-case")]
pub enum Role {
    Reactor,
    Integrator,
    Implementer,
    Reviewer,
    Researcher,
    Decomposer,
    Director,
}
