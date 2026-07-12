//! `ReviewsStore`: typed accessor for `Review` records (Phase 7 of
//! `docs/design/2026-07-11-verified-swarm.md`).
//!
//! Mirrors `NotesStore`: JSONL append-only on disk with an SQLite cache
//! for the `bundle_id` index. Reviews are append-only history — one row
//! per review round, never mutated — so there is no `update` and no OCC
//! lock. Phase 11 (persist reviews + deterministic accept gate) is the
//! first writer; the read path is `list_by_bundle` (indexed `bundle_id`
//! query), which callers use to compute the next round number.

use std::str::FromStr;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};
use tracing::instrument;

use domain::{BundleId, Review, ReviewId};

use crate::error::StoreError;

/// Narrow write+read sink for `Review` evidence, mirroring `CheckRunSink`.
/// Reviews are append-only history (one row per round, never mutated), so
/// this is create + list only — no OCC, no update. `run_reviewer` (Phase 11)
/// persists one `Review` per round through `create_review` after computing
/// the round via `list_reviews_by_bundle`. The `&B` / `Arc<B>` forwarding
/// impls let a caller pass a borrowed sink (the daemon injects
/// `&*self.summary_fanout`).
#[trait_variant::make(Send)]
pub trait ReviewSink: Send + Sync {
    async fn create_review(&self, review: Review) -> Result<ReviewId, StoreError>;
    async fn list_reviews_by_bundle(&self, bundle_id: &BundleId) -> Result<Vec<Review>, StoreError>;
}

impl ReviewSink for crate::Store {
    async fn create_review(&self, review: Review) -> Result<ReviewId, StoreError> {
        self.reviews().create(review).await
    }

    async fn list_reviews_by_bundle(&self, bundle_id: &BundleId) -> Result<Vec<Review>, StoreError> {
        self.reviews().list_by_bundle(bundle_id).await
    }
}

impl<B: ReviewSink + ?Sized> ReviewSink for &B {
    async fn create_review(&self, review: Review) -> Result<ReviewId, StoreError> {
        (*self).create_review(review).await
    }

    async fn list_reviews_by_bundle(&self, bundle_id: &BundleId) -> Result<Vec<Review>, StoreError> {
        (*self).list_reviews_by_bundle(bundle_id).await
    }
}

impl<B: ReviewSink + ?Sized> ReviewSink for std::sync::Arc<B> {
    async fn create_review(&self, review: Review) -> Result<ReviewId, StoreError> {
        (**self).create_review(review).await
    }

    async fn list_reviews_by_bundle(&self, bundle_id: &BundleId) -> Result<Vec<Review>, StoreError> {
        (**self).list_reviews_by_bundle(bundle_id).await
    }
}

pub struct ReviewsStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> ReviewsStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    /// Persist a new Review. `AlreadyExists` if a record with the same id
    /// is already stored — vanishingly unlikely with freshly minted
    /// `ReviewId`s, but the pre-check mirrors the other stores.
    #[instrument(
        name = "reviews.create",
        level = "debug",
        skip_all,
        fields(
            record_kind = "review",
            record_id = %review.id,
            bundle_id = %review.bundle_id,
            round = review.round,
            op = "create",
        ),
        ret,
        err,
    )]
    pub async fn create(&self, review: Review) -> Result<ReviewId, StoreError> {
        let id_str = review.id.as_ref().to_string();
        if self.inner.get::<Review>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "reviews",
                id: id_str,
            });
        }
        let returned = self.inner.create(review).await?;
        Ok(ReviewId::from_str(&returned).expect("ReviewId::from_str is Infallible"))
    }

    /// Fetch a Review by id. Missing id yields `StoreError::RecordNotFound`.
    #[instrument(
        name = "reviews.get",
        level = "debug",
        skip_all,
        fields(record_kind = "review", record_id = %id, op = "get"),
        err,
    )]
    pub async fn get(&self, id: &ReviewId) -> Result<Review, StoreError> {
        match self.inner.get::<Review>(id.as_ref()).await? {
            Some(review) => Ok(review),
            None => Err(StoreError::RecordNotFound {
                collection: "reviews",
                id: id.to_string(),
            }),
        }
    }

    /// Every Review round for the given Bundle. Backed by the SQLite index
    /// on `bundle_id` (`#[record(indexed)]` on the struct field), so this
    /// is an index lookup rather than a full-table scan.
    #[instrument(
        name = "reviews.list_by_bundle",
        level = "debug",
        skip_all,
        fields(record_kind = "review", bundle_id = %bundle_id, op = "list_by_bundle", count = tracing::field::Empty),
        err,
    )]
    pub async fn list_by_bundle(&self, bundle_id: &BundleId) -> Result<Vec<Review>, StoreError> {
        let filter = Filter {
            field: "bundle_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(bundle_id.to_string()),
        };
        let result = self.inner.list::<Review>(&[filter]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }
}
