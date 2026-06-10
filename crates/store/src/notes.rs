//! `NotesStore`: typed accessor for `OperatorNote` records (Phase 7 of
//! `docs/design/2026-05-09-director-phase-2.md`).
//!
//! Mirrors `WorksStore` / `BundlesStore`: JSONL append-only on disk
//! with an SQLite cache for the `plan_id` index. The Director's read
//! path is `list_unread_for_plan` (indexed `plan_id` query + in-memory
//! filter for `read_at.is_none()`), and the write path is
//! `mark_read(ids, ts)` which appends a fresh full-record line per id
//! with `read_at` populated.

use std::str::FromStr;

use taskstore_async::{AsyncStore, Filter, FilterOp, IndexValue};
use tracing::instrument;

use domain::{NoteId, OperatorNote, PlanId};

use crate::error::StoreError;

pub struct NotesStore<'a> {
    inner: &'a AsyncStore,
}

impl<'a> NotesStore<'a> {
    pub(crate) fn new(inner: &'a AsyncStore) -> Self {
        Self { inner }
    }

    /// Persist a new note. `AlreadyExists` if a note with the same id
    /// is already stored — vanishingly unlikely with freshly minted
    /// `NoteId`s, but the pre-check mirrors the other stores.
    #[instrument(
        name = "notes.create",
        level = "debug",
        skip_all,
        fields(record_kind = "note", record_id = %note.id, plan_id = %note.plan_id, op = "create"),
        ret,
        err,
    )]
    pub async fn create(&self, note: OperatorNote) -> Result<NoteId, StoreError> {
        let id_str = note.id.as_ref().to_string();
        if self.inner.get::<OperatorNote>(&id_str).await?.is_some() {
            return Err(StoreError::AlreadyExists {
                collection: "operatornotes",
                id: id_str,
            });
        }
        let returned = self.inner.create(note).await?;
        Ok(NoteId::from_str(&returned).expect("NoteId::from_str is Infallible"))
    }

    /// Fetch a note by id.
    #[instrument(
        name = "notes.get",
        level = "debug",
        skip_all,
        fields(record_kind = "note", record_id = %id, op = "get"),
        err,
    )]
    pub async fn get(&self, id: &NoteId) -> Result<OperatorNote, StoreError> {
        match self.inner.get::<OperatorNote>(id.as_ref()).await? {
            Some(note) => Ok(note),
            None => Err(StoreError::RecordNotFound {
                collection: "operatornotes",
                id: id.to_string(),
            }),
        }
    }

    /// Every note for the given Plan, regardless of read state.
    /// SQLite-indexed via the `#[record(indexed)]` `plan_id` field.
    #[instrument(
        name = "notes.list_by_plan",
        level = "debug",
        skip_all,
        fields(record_kind = "note", plan_id = %plan_id, op = "list_by_plan", count = tracing::field::Empty),
        err,
    )]
    pub async fn list_by_plan(&self, plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError> {
        let filter = Filter {
            field: "plan_id".to_string(),
            op: FilterOp::Eq,
            value: IndexValue::String(plan_id.to_string()),
        };
        let result = self.inner.list::<OperatorNote>(&[filter]).await?;
        tracing::Span::current().record("count", result.len());
        Ok(result)
    }

    /// Unread notes for the given Plan, oldest first by `created_at`.
    /// Backed by `list_by_plan` + an in-memory `read_at.is_none()`
    /// filter; first-gate Plan note counts are sub-100, so a full
    /// indexed scan + filter is cheaper than a compound SQL query.
    #[instrument(
        name = "notes.list_unread_for_plan",
        level = "debug",
        skip_all,
        fields(record_kind = "note", plan_id = %plan_id, op = "list_unread", count = tracing::field::Empty),
        err,
    )]
    pub async fn list_unread_for_plan(&self, plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError> {
        let mut notes: Vec<OperatorNote> = self
            .list_by_plan(plan_id)
            .await?
            .into_iter()
            .filter(|n| n.is_unread())
            .collect();
        notes.sort_by_key(|n| n.created_at);
        tracing::Span::current().record("count", notes.len());
        Ok(notes)
    }

    /// Mark each note in `ids` as read at `ts_ms`. Append-only: writes
    /// a fresh JSONL line per id with `read_at = Some(ts_ms)` and a
    /// monotonically-floored `updated_at`. Last-write-wins on replay.
    ///
    /// No OCC version-check: notes have one writer (the IPC handler
    /// for `create`, the Director task for `mark_read`), and `read_at`
    /// is monotonic `None -> Some`. Missing ids surface as
    /// `StoreError::RecordNotFound` for the first failing id; partial
    /// progress IS persisted for earlier ids in the batch.
    #[instrument(
        name = "notes.mark_read",
        level = "debug",
        skip_all,
        fields(record_kind = "note", op = "mark_read", count = ids.len(), ts_ms),
        err,
    )]
    pub async fn mark_read(&self, ids: &[NoteId], ts_ms: i64) -> Result<(), StoreError> {
        for id in ids {
            let mut note = self.get(id).await?;
            if note.is_unread() {
                let prior = note.updated_at;
                note.mark_read(ts_ms);
                // Monotonic `updated_at` floor. `mark_read` stamps
                // `updated_at` from the caller-supplied `ts_ms`, which can
                // tie (or, under clock skew, regress) against the value
                // already on disk — a hazard for the taskstore merge
                // driver's latest-`updated_at`-wins tie-break on replay.
                // Clamp strictly above the prior value. `read_at` keeps
                // the semantic `ts_ms`; only the bookkeeping field is
                // floored.
                note.updated_at = std::cmp::max(domain::now_millis(), prior + 1);
                self.inner.update(note).await?;
            }
        }
        Ok(())
    }
}
