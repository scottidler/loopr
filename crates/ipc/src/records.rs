//! `RecordList` / `RecordGet` wire types.
//!
//! `RecordList` returns lightweight **summary projections** to keep response
//! payloads well under the 1 MiB [`MAX_LINE_BYTES`] frame cap even on mature
//! repos with hundreds of Works/Bundles. `RecordGet` returns the full record
//! for detail views. See `docs/design/2026-04-23-cli-plumbing-shape.md`.
//!
//! Adding Spec/Phase later is mechanical: two new variants in [`RecordKind`],
//! two new summary structs, two new arms in [`RecordsResult`] / [`RecordResult`],
//! two new handler branches on the daemon side.
//!
//! [`MAX_LINE_BYTES`]: crate::MAX_LINE_BYTES

use serde::{Deserialize, Serialize};

use domain::{Bundle, BundleId, BundleStatus, Plan, PlanId, PlanStatus, Tick, TickId, Work, WorkId, WorkStatus};

/// Discriminator for `record.list` requests. The wire form is kebab-case
/// (e.g. `"plan"`, `"work"`), not PascalCase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub enum RecordKind {
    Plan,
    Work,
    Bundle,
    Tick,
    // Spec, Phase land here when those records ship.
}

/// Params for `record.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordListParams {
    pub kind: RecordKind,
}

/// Params for `record.get`. The id's 2-char prefix (e.g. `"pl"`, `"wk"`)
/// picks the record type on the daemon side; see
/// `crates/domain/src/id.rs` for the canonical prefix-to-type mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordGetParams {
    pub id: String,
}

// ---------- Summary projections ----------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSummary {
    pub id: PlanId,
    pub goal: String,
    pub status: PlanStatus,
    pub updated_at: i64,
}

impl From<&Plan> for PlanSummary {
    fn from(p: &Plan) -> Self {
        Self {
            id: p.id.clone(),
            goal: p.goal.clone(),
            status: p.status,
            updated_at: p.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSummary {
    pub id: WorkId,
    pub parent_id: PlanId,
    pub title: String,
    pub status: WorkStatus,
    pub updated_at: i64,
}

impl From<&Work> for WorkSummary {
    fn from(w: &Work) -> Self {
        Self {
            id: w.id.clone(),
            parent_id: w.parent_id.clone(),
            title: w.title.clone(),
            status: w.status,
            updated_at: w.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleSummary {
    pub id: BundleId,
    pub work_id: WorkId,
    pub status: BundleStatus,
    pub updated_at: i64,
}

impl From<&Bundle> for BundleSummary {
    fn from(b: &Bundle) -> Self {
        Self {
            id: b.id.clone(),
            work_id: b.work_id.clone(),
            status: b.status,
            updated_at: b.updated_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TickSummary {
    pub id: TickId,
    pub plan_id: PlanId,
    pub sha: String,
    pub updated_at: i64,
}

impl From<&Tick> for TickSummary {
    fn from(t: &Tick) -> Self {
        Self {
            id: t.id.clone(),
            plan_id: t.plan_id.clone(),
            sha: t.sha.clone(),
            updated_at: t.updated_at,
        }
    }
}

// ---------- Result enums ----------
//
// Adjacent tagging (`#[serde(tag = "kind", content = "...")]`) lets us use
// tuple variants with `Vec<T>` inner types; internal tagging would require
// struct variants because a Vec serializes as a JSON array, which has no
// room for an internally-tagged discriminator.

/// Success payload for `record.list`. Carries per-kind summary vectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "records", rename_all = "kebab-case")]
pub enum RecordsResult {
    Plans(Vec<PlanSummary>),
    Works(Vec<WorkSummary>),
    Bundles(Vec<BundleSummary>),
    Ticks(Vec<TickSummary>),
}

/// Success payload for `record.get`. Carries the full record for the
/// requested id. Not `PartialEq`: `Plan`/`Work`/`Bundle`/`Tick` do not
/// impl `PartialEq` because their `updated_at`/`created_at` fields make
/// equality comparisons slippery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "kebab-case")]
pub enum RecordResult {
    Plan(Plan),
    Work(Work),
    Bundle(Bundle),
    Tick(Tick),
}
