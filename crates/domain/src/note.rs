//! `OperatorNote` record type: persistent operator-to-Director chat
//! messages.
//!
//! Phase 7 of `docs/design/2026-05-09-director-phase-2.md`. The
//! operator submits a note via `loopr director chat <plan-id> "<msg>"`;
//! the daemon's IPC handler persists it via `NotesStore`, then notifies
//! the per-Plan Director task which prepends unread notes to its user
//! prompt on the next iteration. Unread vs read is a boolean derived
//! from `read_at`: `None` is unread, `Some(timestamp_ms)` is read.
//!
//! No FSM: notes are append-only, with a single one-way `read_at`
//! transition. Concurrent writers are not a concern (one IPC handler
//! creates, one Director task marks read; both serialize through the
//! TaskStore writer dispatcher).

use serde::{Deserialize, Serialize};

use derive::Record;

use crate::id::{NoteId, PlanId, now_millis};

/// Operator-submitted message routed into the Director's user prompt.
/// Stored at `<target>/.loopr/taskstore/operatornotes.jsonl` via the
/// `Record` derive (the on-disk filename is the struct ident
/// lowercased and pluralized; no snake_case transform).
#[derive(Debug, Clone, Serialize, Deserialize, Record)]
#[serde(deny_unknown_fields)]
pub struct OperatorNote {
    pub id: NoteId,
    /// Foreign key: which Plan's Director should receive this note.
    /// Indexed so `NotesStore::list_by_plan` is an SQLite index lookup
    /// rather than a full-table scan.
    #[record(indexed)]
    pub plan_id: PlanId,
    pub updated_at: i64,
    pub created_at: i64,
    /// Submitter identity (env `USER` at the time the IPC handler
    /// ran). Surfaced in the user prompt so the LLM can attribute
    /// operator advice if multiple humans interact with one Plan.
    pub author: String,
    /// Operator-supplied text. IPC handler caps at 4 KB with a
    /// truncation marker if longer; this field carries the post-cap
    /// payload.
    pub message: String,
    /// `None` until the Director's iteration ingests this note;
    /// `Some(timestamp_ms)` after. `NotesStore::list_unread_for_plan`
    /// filters on this field in-memory after the indexed `plan_id`
    /// query returns.
    #[serde(default)]
    pub read_at: Option<i64>,
}

impl OperatorNote {
    /// Construct a fresh note in the unread state. The IPC handler is
    /// the sole production caller; tests build notes directly.
    pub fn new(plan_id: PlanId, author: String, message: String) -> Self {
        let now = now_millis();
        Self {
            id: NoteId::new(),
            plan_id,
            updated_at: now,
            created_at: now,
            author,
            message,
            read_at: None,
        }
    }

    /// Mark this in-memory note as read at the given timestamp. The
    /// caller is responsible for persisting the change via
    /// `NotesStore::mark_read`. `updated_at` advances so taskstore's
    /// SQLite cache reflects the new state on the next list query.
    pub fn mark_read(&mut self, ts_ms: i64) {
        self.read_at = Some(ts_ms);
        self.updated_at = ts_ms;
    }

    /// Convenience predicate. `read_at.is_none()` is the canonical
    /// check; this is sugar for filter expressions.
    pub fn is_unread(&self) -> bool {
        self.read_at.is_none()
    }
}

#[cfg(test)]
mod tests;
