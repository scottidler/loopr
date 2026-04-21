//! `AcceptanceCriteria` — a list of assertable statements attached to a
//! record to define what "done" means. Shared by `Work` today; later
//! stages (`Spec`, `Phase`) lift the same shape when they land.
//!
//! The newtype is `#[serde(transparent)]` so the on-wire representation
//! is a bare JSON array of strings (`["criterion-1", "criterion-2"]`),
//! not a map-keyed shape. The inner `Vec<String>` is `pub` so call sites
//! like the decomposer construct an `AcceptanceCriteria` directly
//! (`AcceptanceCriteria(vec)`) without needing a `From<Vec<String>>`
//! impl.

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AcceptanceCriteria(pub Vec<String>);

impl AcceptanceCriteria {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, String> {
        self.0.iter()
    }
}
