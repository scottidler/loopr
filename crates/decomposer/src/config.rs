//! `DecomposerConfig`: this crate's own knob bag, composed into the
//! top-level loopr `Config` as `decomposer.*`.

use serde::{Deserialize, Serialize};

/// Decomposition knobs. Keys on disk are kebab-case
/// (`decomposer.max-children`).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct DecomposerConfig {
    /// Maximum child Works a single decomposition may produce. The
    /// handler spawns an Implementer per unblocked Work with no pool cap,
    /// so an unbounded decomposition (e.g. 50 children) would fan out 50
    /// concurrent agents. A decomposition exceeding this bound is a
    /// validation error that triggers one retry-with-error before bailing
    /// (same path as the other post-parse validation errors). Default 10.
    pub max_children: usize,
}

impl Default for DecomposerConfig {
    fn default() -> Self {
        Self { max_children: 10 }
    }
}
