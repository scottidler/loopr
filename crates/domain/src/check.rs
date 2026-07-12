//! `CheckRun` record type: the persisted evidence of one executed check
//! command (a Reviewer or Integrator running `cargo test`, `cargo clippy`,
//! etc. against a Bundle's checkout).
//!
//! Phase 7 of `docs/design/2026-07-11-verified-swarm.md` introduces the
//! type and its `CheckRunsStore`; Phase 10 (Reviewer executed checks) is
//! the first writer, persisting one `CheckRun` per command and referencing
//! them from the round's `Review`. This phase is purely additive: the
//! record, its typed id, and the store handle land with zero consumers.
//!
//! No FSM: a `CheckRun` is an immutable, append-only fact. It records what
//! was executed and what came back; it is never transitioned. The truth of
//! a Bundle's verification moves out of the free-text `Bundle.verification`
//! string and onto these structured `CheckRun` / `Review` records.

use serde::{Deserialize, Serialize};

use derive::Record;

use crate::Role;
use crate::id::{BundleId, CheckRunId, WorkId, now_millis};

/// One executed check command and its outcome. Persisted at
/// `<target>/.loopr/taskstore/checkruns.jsonl` via the `Record` derive
/// (the on-disk filename is the struct ident lowercased and pluralized).
///
/// The `output_digest` (sha256 of the combined stdout+stderr) is the
/// tamper-evident fingerprint of the full output; `output_excerpt` is a
/// capped tail carried for human/LLM readability without persisting an
/// unbounded blob. Both are written by the check runner (Phase 10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct CheckRun {
    pub id: CheckRunId,
    /// Foreign key: which Bundle this check evaluated. Indexed so
    /// `CheckRunsStore::list_by_bundle` is an SQLite index lookup rather
    /// than a full-table scan.
    #[record(indexed)]
    pub bundle_id: BundleId,
    /// Foreign key: the Work whose Bundle this is. Carried for
    /// attribution and future per-Work queries; not indexed this phase
    /// (no `list_by_work` accessor exists yet — added when a caller needs
    /// it, per "defer capacity features until they're an observed
    /// problem").
    pub work_id: WorkId,
    pub updated_at: i64,
    pub created_at: i64,
    /// The command as executed (the resolved argv joined for display),
    /// e.g. `cargo test --workspace`.
    pub command: String,
    /// Process exit code. `0` is success; nonzero is a code signal the
    /// deterministic accept gate (Phase 10/11) acts on.
    pub exit_code: i32,
    /// sha256 hex digest of the combined stdout+stderr output — the
    /// tamper-evident fingerprint of the full result.
    pub output_digest: String,
    /// Capped tail of the combined output, for human/LLM inspection
    /// without persisting an unbounded blob.
    pub output_excerpt: String,
    /// Which role ran the check. `Reviewer` for pre-verdict checks,
    /// `Integrator` for post-merge validation.
    pub executor: Role,
    /// Wall-clock duration of the command in milliseconds.
    pub duration_ms: u64,
}

impl CheckRun {
    /// Construct a fresh `CheckRun`: fresh `CheckRunId`, `created_at ==
    /// updated_at == now`. The check runner (Phase 10) is the sole
    /// production caller, passing the executed command plus its captured
    /// outcome; tests build records directly or via this seam.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bundle_id: BundleId,
        work_id: WorkId,
        command: String,
        exit_code: i32,
        output_digest: String,
        output_excerpt: String,
        executor: Role,
        duration_ms: u64,
    ) -> Self {
        let now = now_millis();
        Self {
            id: CheckRunId::new(),
            bundle_id,
            work_id,
            updated_at: now,
            created_at: now,
            command,
            exit_code,
            output_digest,
            output_excerpt,
            executor,
            duration_ms,
        }
    }

    /// A check succeeded iff its process exited `0`. Sugar for the
    /// deterministic accept gate's red/green partition.
    pub fn passed(&self) -> bool {
        self.exit_code == 0
    }
}

#[cfg(test)]
mod tests;
