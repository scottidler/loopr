//! `TicksStore`: append-only persistence for `Tick` records.
//!
//! A Tick is produced by the Integrator when one or more Accepted Bundles have
//! been merged into a Plan's integration branch. First gate: one Bundle per
//! Tick. Ticks are immutable; there is no `update` method.
//!
//! `create` is serialized by an intra-daemon `tick_lock: tokio::sync::Mutex<()>`
//! so the duplicate-detection read-check-write is race-free. Without it, two
//! concurrent `integrate` calls performing the crash-recovery idempotency
//! dance could both pass `list_by_plan_id` returning empty and both append,
//! producing two Ticks for one merge. The lock shape mirrors
//! `BundlesStore::update_lock` introduced in the Reviewer stage.
//!
//! `DuplicateTick` detection compares the incoming `(plan_id, bundles-as-set)`
//! against `list_by_plan_id`; if any existing Tick's bundles set matches, the
//! method returns `StoreError::DuplicateTick { tick_id, plan_id, bundles }`
//! (carrying the existing Tick's id) without appending. The Integrator's
//! crash-recovery path uses this as the signal to short-circuit Phase 3 and
//! return `Ok(existing_tick)` with one extra `TicksStore::get` call.

use std::collections::HashSet;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};
use tokio::sync::Mutex;

use domain::{PlanId, Tick, TickId};

use crate::error::StoreError;

pub struct TicksStore<'a> {
    inner: &'a AsyncStore,
    tick_lock: &'a Mutex<()>,
}

impl<'a> TicksStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore, tick_lock: &'a Mutex<()>) -> Self {
        Self { inner, tick_lock }
    }

    /// Persist a new Tick with intra-daemon duplicate-detection.
    ///
    /// Sequence under `tick_lock`:
    /// 1. acquire `tick_lock`,
    /// 2. list every Tick for `tick.plan_id`,
    /// 3. if any existing Tick's `bundles` (as a set) matches the incoming
    ///    `tick.bundles`, return `DuplicateTick { tick_id: existing.id, .. }`
    ///    without appending,
    /// 4. otherwise append via `AsyncStore::create`,
    /// 5. drop the lock.
    ///
    /// The comparison is set-based (`HashSet`), not Vec-order-sensitive, so a
    /// caller reordering the bundles Vec does not produce a false non-duplicate.
    ///
    /// On success returns a clone of the input Tick (caller-convenience so
    /// the call site need not clone upstream of the `await`).
    pub async fn create(&self, tick: Tick) -> Result<Tick, StoreError> {
        let _guard = self.tick_lock.lock().await;

        let existing = self.list_by_plan_id_locked(&tick.plan_id).await?;
        let incoming_bundles: HashSet<_> = tick.bundles.iter().cloned().collect();
        if let Some(dup) = existing.iter().find(|t| {
            let t_bundles: HashSet<_> = t.bundles.iter().cloned().collect();
            t_bundles == incoming_bundles
        }) {
            return Err(StoreError::DuplicateTick {
                tick_id: dup.id.clone(),
                plan_id: tick.plan_id.clone(),
                bundles: tick.bundles.clone(),
            });
        }

        self.inner.create(tick.clone()).await?;
        Ok(tick)
    }

    /// Fetch a Tick by id. Missing id yields `StoreError::RecordNotFound`.
    pub async fn get(&self, id: &TickId) -> Result<Tick, StoreError> {
        match self.inner.get::<Tick>(id.as_ref()).await? {
            Some(tick) => Ok(tick),
            None => Err(StoreError::RecordNotFound {
                collection: "ticks",
                id: id.to_string(),
            }),
        }
    }

    /// Return every Tick for the given `PlanId`, ordered by `updated_at`
    /// descending per `AsyncStore::list`'s contract. Backed by the SQLite
    /// index on `plan_id` (`#[record(indexed)]` on the struct field).
    pub async fn list_by_plan_id(&self, plan_id: &PlanId) -> Result<Vec<Tick>, StoreError> {
        let filter = Filter {
            field: "plan_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(plan_id.to_string()),
        };
        Ok(self.inner.list::<Tick>(&[filter]).await?)
    }

    /// Internal variant used inside `create`'s lock scope; identical query
    /// shape but does not re-acquire the lock.
    async fn list_by_plan_id_locked(&self, plan_id: &PlanId) -> Result<Vec<Tick>, StoreError> {
        let filter = Filter {
            field: "plan_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(plan_id.to_string()),
        };
        Ok(self.inner.list::<Tick>(&[filter]).await?)
    }
}

#[cfg(test)]
mod tests;
