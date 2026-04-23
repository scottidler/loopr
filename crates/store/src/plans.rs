use std::str::FromStr;

use taskstore_async::AsyncStore;

use domain::{Plan, PlanId};

use crate::error::StoreError;

pub struct PlansStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> PlansStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    /// Persist a new Plan. Errors with `AlreadyExists` if a plan with the
    /// same id is already stored.
    ///
    /// This enforces the domain contract of `create` (no-overwrite) at the
    /// anti-corruption boundary. The underlying `taskstore_async::AsyncStore`
    /// inherits SQLite's `INSERT OR REPLACE` semantics and would silently
    /// overwrite; the pre-check `get` here converts that into an explicit
    /// `StoreError::AlreadyExists`.
    ///
    /// **Race-condition caveat:** the pre-check is not transactional. A
    /// concurrent create of the same id between the `get` and the upstream
    /// `create` would still overwrite. Stage 5's single-daemon model mints
    /// fresh random `PlanId`s via `Plan::new()`, making collision ~0. When a
    /// multi-writer scenario emerges, extend `AsyncStore` with a conditional-
    /// write primitive and replace this pre-check with the atomic upstream
    /// path.
    pub async fn create(&self, plan: Plan) -> Result<PlanId, StoreError> {
        let id_str = plan.id.as_ref().to_string();
        if self.inner.get::<Plan>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "plans",
                id: id_str,
            });
        }
        let returned = self.inner.create(plan).await?;
        Ok(PlanId::from_str(&returned).expect("PlanId::from_str is Infallible"))
    }

    /// Fetch a Plan by id. Missing id yields `StoreError::RecordNotFound`;
    /// the `Option<T>` from the underlying store is collapsed here so every
    /// Stage 5+ accessor returns the same shape.
    pub async fn get(&self, id: &PlanId) -> Result<Plan, StoreError> {
        match self.inner.get::<Plan>(id.as_ref()).await? {
            Some(plan) => Ok(plan),
            None => Err(StoreError::RecordNotFound {
                collection: "plans",
                id: id.to_string(),
            }),
        }
    }

    /// Return every stored Plan. `AsyncStore::list` orders by `updated_at`
    /// descending; callers should not depend on order beyond that contract.
    pub async fn list(&self) -> Result<Vec<Plan>, StoreError> {
        Ok(self.inner.list::<Plan>(&[]).await?)
    }

    /// Persist a status / field change on an existing Plan. Delegates to
    /// `AsyncStore::update`, which rewrites the JSONL line and refreshes the
    /// SQLite cache row. Consumed by the Stage 8 wiring capstone's Integrator
    /// spawn: after every child Work under a Plan is terminal with at least
    /// one Done, the Coordinator fires `Active -> Complete` via this method.
    /// Mirrors `WorksStore::update` (blind-write, no OCC); Plans have no
    /// concurrent-writer race in the single-daemon-per-target threat model.
    pub async fn update(&self, plan: Plan) -> Result<(), StoreError> {
        self.inner.update(plan).await?;
        Ok(())
    }
}
