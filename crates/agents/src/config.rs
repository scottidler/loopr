//! `ImplementerConfig`: flat knob bag for the Implementer loop.
//!
//! One struct, no trait, no nested substructs. The design doc
//! alternative (`RetryStrategy` trait with `MaxAttemptsRetry` as the
//! default impl) was rejected: one concrete shape, no runtime-swap
//! point, no reason for the indirection.

#[derive(Debug, Clone)]
pub struct ImplementerConfig {
    /// Hard upper bound on outer loop iterations per Work. Hitting
    /// this triggers the force-propose path (or its guard-escalate
    /// alternative).
    pub max_iterations: u32,

    /// Maximum LLM re-prompts within the self-correction sub-loop
    /// for a single iteration.
    pub max_requeries: u32,

    /// Consecutive full-iteration parse failures before the Lifeguard
    /// escalates. Distinct from `max_iterations`: this fires only
    /// when every requery in an iteration also fails.
    pub max_parse_failures: u32,

    /// Consecutive identical actions (structurally canonical hash)
    /// before the Lifeguard escalates the action-repeat path.
    pub max_repeat_action: u32,

    /// Force-propose guard: if more than this many tracked files are
    /// modified at iteration cap, escalate instead of committing.
    pub max_force_propose_files: u32,

    /// Force-propose guard: if any single staged file exceeds this
    /// size in bytes at iteration cap, escalate instead of committing.
    pub max_force_propose_file_size_bytes: u64,
}

impl Default for ImplementerConfig {
    fn default() -> Self {
        Self {
            max_iterations: 20,
            max_requeries: 3,
            max_parse_failures: 5,
            max_repeat_action: 3,
            max_force_propose_files: 100,
            max_force_propose_file_size_bytes: 10 * 1024 * 1024,
        }
    }
}
