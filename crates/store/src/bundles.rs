use std::str::FromStr;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};

use domain::{Bundle, BundleId, WorkId};

use crate::error::StoreError;

pub struct BundlesStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> BundlesStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    /// Persist a new Bundle. Errors with `AlreadyExists` if a bundle
    /// with the same id is already stored. Mirrors `PlansStore::create`
    /// and `WorksStore::create` race-condition caveat: the pre-check
    /// is not transactional, but freshly minted `BundleId`s make
    /// collisions ~0 under the single-daemon model.
    pub async fn create(&self, bundle: Bundle) -> Result<BundleId, StoreError> {
        let id_str = bundle.id.as_ref().to_string();
        if self.inner.get::<Bundle>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "bundles",
                id: id_str,
            });
        }
        let returned = self.inner.create(bundle).await?;
        Ok(BundleId::from_str(&returned).expect("BundleId::from_str is Infallible"))
    }

    /// Fetch a Bundle by id. Missing id yields `StoreError::RecordNotFound`.
    pub async fn get(&self, id: &BundleId) -> Result<Bundle, StoreError> {
        match self.inner.get::<Bundle>(id.as_ref()).await? {
            Some(bundle) => Ok(bundle),
            None => Err(StoreError::RecordNotFound {
                collection: "bundles",
                id: id.to_string(),
            }),
        }
    }

    /// Return every stored Bundle. `AsyncStore::list` orders by
    /// `updated_at` descending; callers should not depend on order
    /// beyond that contract.
    pub async fn list(&self) -> Result<Vec<Bundle>, StoreError> {
        Ok(self.inner.list::<Bundle>(&[]).await?)
    }

    /// Return every Bundle whose `work_id` matches the given
    /// `WorkId`. Backed by the SQLite index on `work_id`
    /// (`#[record(indexed)]` on the struct field), so this is an
    /// index lookup rather than a full-table scan.
    pub async fn list_by_work_id(&self, work_id: &WorkId) -> Result<Vec<Bundle>, StoreError> {
        let filter = Filter {
            field: "work_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(work_id.to_string()),
        };
        Ok(self.inner.list::<Bundle>(&[filter]).await?)
    }
}
