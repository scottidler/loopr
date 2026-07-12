//! `AcceptanceCriteria` — a list of assertable statements attached to a
//! record to define what "done" means. Shared by `Work` today; later
//! stages (`Spec`, `Phase`) lift the same shape when they land.
//!
//! Phase 8 of `docs/design/2026-07-11-verified-swarm.md` turns each
//! criterion from a bare `String` into a `Criterion { id, text }`: the
//! decomposer mints a stable `id` per criterion at decompose time, and the
//! Reviewer keys per-criterion evidence (`CriterionResult`) on that `id`
//! instead of fuzzy-matching the criterion's words against freeform review
//! text.
//!
//! On-wire the newtype is a bare JSON array — but of criterion objects
//! now (`[{"id":1,"text":"…"}]`), not strings. A hand-written
//! `Deserialize` keeps **back-compat with the old string-array form**
//! (`["criterion-1","criterion-2"]`): pre-Phase-8 `works.jsonl` rows still
//! load, their entries becoming `Criterion`s with sequential ids (1, 2, …).
//! The inner `Vec<Criterion>` is `pub` so call sites can pattern-match and
//! construct directly; `from_texts` is the ergonomic constructor that mints
//! the sequential ids for callers holding a `Vec<String>` (the decomposer,
//! tests).

use serde::{Deserialize, Deserializer, Serialize};

/// A single acceptance criterion: a stable `id` (minted at decompose time,
/// referenced by `CriterionResult::criterion_id`) plus the assertable
/// `text`. `id` is structural, not free-form — two criteria in one list
/// never share an id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Criterion {
    pub id: u32,
    pub text: String,
}

/// A `Work`'s acceptance criteria. Serializes transparently as a bare JSON
/// array of `Criterion` objects; deserializes from **either** that shape or
/// the legacy bare-string array (see the module docs and the custom
/// `Deserialize` impl below).
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct AcceptanceCriteria(pub Vec<Criterion>);

impl AcceptanceCriteria {
    /// Build criteria from a list of texts, minting sequential 1-based ids
    /// (`texts[0]` -> id 1, `texts[1]` -> id 2, …). The decomposer's mint
    /// point at decompose time, and the ergonomic constructor for tests
    /// that previously wrote `AcceptanceCriteria(vec!["…"])`.
    pub fn from_texts(texts: Vec<String>) -> Self {
        let mut id: u32 = 0;
        let criteria = texts
            .into_iter()
            .map(|text| {
                id += 1;
                Criterion { id, text }
            })
            .collect();
        Self(criteria)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Criterion> {
        self.0.iter()
    }
}

impl<'de> Deserialize<'de> for AcceptanceCriteria {
    /// Accept both the current `[{"id":…,"text":…}]` shape and the legacy
    /// `["…","…"]` string-array shape. Each element is probed with an
    /// untagged helper: a JSON object deserializes as `Structured` (keeping
    /// its own id), a JSON string as `Text` (assigned the next sequential
    /// id). Sequential ids continue one past any explicit id already seen,
    /// so a mixed array never silently reuses one.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum CriterionRepr {
            Structured { id: u32, text: String },
            Text(String),
        }

        let raw = Vec::<CriterionRepr>::deserialize(deserializer)?;
        let mut criteria = Vec::with_capacity(raw.len());
        let mut next_id: u32 = 1;
        for repr in raw {
            let criterion = match repr {
                CriterionRepr::Structured { id, text } => Criterion { id, text },
                CriterionRepr::Text(text) => Criterion { id: next_id, text },
            };
            next_id = criterion.id.saturating_add(1);
            criteria.push(criterion);
        }
        Ok(Self(criteria))
    }
}

#[cfg(test)]
mod tests;
