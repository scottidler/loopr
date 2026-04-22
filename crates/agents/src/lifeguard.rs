//! `Lifeguard`: two independent shutdown paths for the Implementer loop.
//!
//! - `check_action`: detects the LLM repeating an identical action
//!   across iterations. After `max_repeat` consecutive identical
//!   actions, escalates.
//! - `record_parse_failure` / `reset_parse_failures`: tracks how
//!   many consecutive iterations exhausted their requery budget
//!   without a parseable response. After `max_parse_failures`,
//!   escalates.
//!
//! Critical invariant: `reset_parse_failures` is called ONLY on a
//! successful parse (`Ok(actions)` branch of the sub-loop), never
//! unconditionally after the sub-loop exits. Calling it
//! unconditionally would stick the counter at 0 or 1 and the
//! escalation path would be unreachable.
//!
//! Hash stability: actions are hashed via structural canonicalization
//! (keys sorted into a `BTreeMap` before `serde_json::to_string`),
//! not via raw `to_string` of the `serde_json::Value`. Cargo
//! features are additive; if any transitive workspace dep enables
//! `serde_json/preserve_order`, a raw `to_string` would emit keys
//! in insertion order and the dedupe hash would become unstable
//! across LLM key-emission order. Canonicalization removes that
//! dependency on the feature flag.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hasher;

use serde_json::Value;

use crate::action::AgentAction;

#[derive(Debug, Clone)]
pub enum Verdict {
    Continue,
    Escalate(String),
}

pub struct Lifeguard {
    action_counts: HashMap<u64, u32>,
    last_hash: Option<u64>,
    consecutive_parse_failures: u32,
    max_repeat: u32,
    max_parse_failures: u32,
}

impl Lifeguard {
    pub fn new(max_repeat: u32, max_parse_failures: u32) -> Self {
        Self {
            action_counts: HashMap::new(),
            last_hash: None,
            consecutive_parse_failures: 0,
            max_repeat,
            max_parse_failures,
        }
    }

    /// Hash the action structurally and record the occurrence. If
    /// this is the same hash as the last recorded action and the
    /// count has hit `max_repeat`, return Escalate. Otherwise
    /// Continue.
    pub fn check_action(&mut self, action: &AgentAction) -> Verdict {
        let hash = canonical_hash(action);
        let count = self.action_counts.entry(hash).or_insert(0);
        *count += 1;
        let my_count = *count;
        self.last_hash = Some(hash);
        if my_count >= self.max_repeat {
            return Verdict::Escalate(format!(
                "same action repeated {my_count} times (max_repeat={})",
                self.max_repeat
            ));
        }
        Verdict::Continue
    }

    /// Called when the self-correction sub-loop exhausts its
    /// requery budget without producing a parseable response.
    /// Returns Escalate once `max_parse_failures` consecutive
    /// iterations have done so.
    pub fn record_parse_failure(&mut self) -> Verdict {
        self.consecutive_parse_failures += 1;
        if self.consecutive_parse_failures >= self.max_parse_failures {
            Verdict::Escalate(format!(
                "LLM produced unparseable output for {} consecutive iterations (max_parse_failures={})",
                self.consecutive_parse_failures, self.max_parse_failures
            ))
        } else {
            Verdict::Continue
        }
    }

    /// Reset the parse-failure counter. MUST be called only on a
    /// successful parse, never unconditionally.
    pub fn reset_parse_failures(&mut self) {
        self.consecutive_parse_failures = 0;
    }
}

/// FNV-1a 64-bit hash of the action's canonical JSON form.
/// Canonical = keys recursively sorted into `BTreeMap` before
/// serialization. Independent of whether the workspace enables
/// `serde_json/preserve_order`.
pub fn canonical_hash(action: &AgentAction) -> u64 {
    let json = serde_json::to_value(action).expect("AgentAction serializes to Value");
    let canonical = canonicalize(json);
    let s = serde_json::to_string(&canonical).expect("canonicalized Value serializes");
    let mut hasher = FnvHasher::default();
    hasher.write(s.as_bytes());
    hasher.finish()
}

/// Recursively canonicalize a `serde_json::Value`. Objects become
/// `Map` (backed by `BTreeMap` when `preserve_order` is off, but we
/// force ordering explicitly via a sorted `Vec` re-insertion so
/// the feature flag cannot affect the output).
fn canonicalize(v: Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut sorted = BTreeMap::new();
            for (k, val) in map {
                sorted.insert(k, canonicalize(val));
            }
            let rebuilt: serde_json::Map<String, Value> = sorted.into_iter().collect();
            Value::Object(rebuilt)
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize).collect()),
        other => other,
    }
}

/// Minimal FNV-1a implementation. Pinned here to avoid a crate
/// dep for one hash; the algorithm is short and stable.
#[derive(Default)]
struct FnvHasher {
    state: u64,
}

impl FnvHasher {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
}

impl Hasher for FnvHasher {
    fn finish(&self) -> u64 {
        if self.state == 0 { Self::FNV_OFFSET } else { self.state }
    }
    fn write(&mut self, bytes: &[u8]) {
        let mut s = if self.state == 0 { Self::FNV_OFFSET } else { self.state };
        for &b in bytes {
            s ^= u64::from(b);
            s = s.wrapping_mul(Self::FNV_PRIME);
        }
        self.state = s;
    }
}

#[cfg(test)]
mod tests;
