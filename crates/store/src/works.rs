use std::str::FromStr;

use taskstore_async::AsyncStore;

use domain::{Work, WorkId};

use crate::error::StoreError;

pub struct WorksStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> WorksStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    /// Persist a new Work. Errors with `AlreadyExists` if a work with the
    /// same id is already stored. Mirrors `PlansStore::create` including the
    /// race-condition caveat documented there — pre-check is not
    /// transactional, but freshly minted `WorkId`s make collisions ~0 under
    /// the single-daemon model.
    pub async fn create(&self, work: Work) -> Result<WorkId, StoreError> {
        let id_str = work.id.as_ref().to_string();
        if self.inner.get::<Work>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "works",
                id: id_str,
            });
        }
        let returned = self.inner.create(work).await?;
        Ok(WorkId::from_str(&returned).expect("WorkId::from_str is Infallible"))
    }

    /// Batch-create. Delegates to `AsyncStore::create_many`, which inserts
    /// the SQLite cache rows and appends the JSONL truth lines as a single
    /// dispatched writer operation. Per scope memo D10, atomic cross-batch
    /// persistence across *different* JSONL files (e.g. plans + works) is
    /// Stage 7's reconcile-on-restart concern; within a single collection
    /// the writer-thread primitive is atomic.
    ///
    /// A fresh decomposition never has id collisions because `WorkId`s are
    /// freshly minted by the decomposer, so this method's practical failure
    /// mode is limited to IO errors on disk.
    pub async fn create_many(&self, works: Vec<Work>) -> Result<Vec<WorkId>, StoreError> {
        let returned = self.inner.create_many(works).await?;
        Ok(returned
            .into_iter()
            .map(|s| WorkId::from_str(&s).expect("WorkId::from_str is Infallible"))
            .collect())
    }

    /// Fetch a Work by id. Missing id yields `StoreError::RecordNotFound`.
    pub async fn get(&self, id: &WorkId) -> Result<Work, StoreError> {
        match self.inner.get::<Work>(id.as_ref()).await? {
            Some(work) => Ok(work),
            None => Err(StoreError::RecordNotFound {
                collection: "works",
                id: id.to_string(),
            }),
        }
    }

    /// Return every stored Work. `AsyncStore::list` orders by `updated_at`
    /// descending; callers should not depend on order beyond that contract.
    pub async fn list(&self) -> Result<Vec<Work>, StoreError> {
        Ok(self.inner.list::<Work>(&[]).await?)
    }

    /// Persist a status / field change on an existing Work. Delegates to
    /// `AsyncStore::update` which rewrites the JSONL line and refreshes
    /// the SQLite cache row. The Stage-7 wiring requires this so
    /// implementer-error transitions (Blocked / Failed) survive daemon
    /// restart; without it the daemon re-dispatches failing Works forever.
    pub async fn update(&self, work: Work) -> Result<(), StoreError> {
        self.inner.update(work).await?;
        Ok(())
    }
}
