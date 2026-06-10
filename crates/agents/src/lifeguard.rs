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

use std::collections::BTreeMap;
use std::hash::Hasher;

use serde_json::Value;
use tracing::{debug, instrument};

use crate::action::AgentAction;

#[derive(Debug, Clone)]
pub enum Decision {
    Continue,
    Escalate(String),
}

pub struct Lifeguard {
    /// Hash of the immediately preceding action. `None` before the first
    /// `check_action`. Load-bearing: the consecutive-streak detector
    /// compares each new action against it.
    last_hash: Option<u64>,
    /// Length of the current run of consecutive-identical actions. Reset
    /// to 1 whenever a different action arrives. This is CONSECUTIVE, not
    /// cumulative-per-hash: a legitimately repeated `cargo test` after
    /// distinct edits (A, B, A, B, A) never reaches the threshold because
    /// each repeat is interrupted.
    consecutive_count: u32,
    consecutive_parse_failures: u32,
    max_repeat: u32,
    max_parse_failures: u32,
}

impl Lifeguard {
    pub fn new(max_repeat: u32, max_parse_failures: u32) -> Self {
        Self {
            last_hash: None,
            consecutive_count: 0,
            consecutive_parse_failures: 0,
            max_repeat,
            max_parse_failures,
        }
    }

    /// Hash the action structurally and update the consecutive-run
    /// counter. If this action equals the immediately preceding one and
    /// the run length has reached `max_repeat`, return Escalate. A
    /// different action resets the run to 1. Otherwise Continue.
    #[instrument(
        level = "debug",
        skip_all,
        fields(
            action_kind = action.kind(),
            action_hash = tracing::field::Empty,
            action_count = tracing::field::Empty,
            max_repeat = self.max_repeat,
        ),
        ret,
    )]
    pub fn check_action(&mut self, action: &AgentAction) -> Decision {
        let span = tracing::Span::current();
        let hash = canonical_hash(action);
        span.record("action_hash", hash);
        // Consecutive-run semantics keyed off `last_hash`: same as the
        // previous action -> extend the run; different -> reset to 1.
        let my_count = if self.last_hash == Some(hash) {
            self.consecutive_count + 1
        } else {
            1
        };
        self.consecutive_count = my_count;
        self.last_hash = Some(hash);
        span.record("action_count", my_count);
        debug!(
            action_kind = action.kind(),
            action_hash = format!("{hash:#018x}"),
            action_count = my_count,
            max_repeat = self.max_repeat,
            "lifeguard: action observed"
        );
        if my_count >= self.max_repeat {
            // Embed action_hash + action_count in the message itself. The
            // span fields exist (above), but at INFO-level filtering the
            // span open/close events are dropped while the implementer's
            // ERROR event survives — so the operator only sees this line.
            // Without action_hash here, "which action repeated?" requires
            // re-running at DEBUG. With it here, the answer is in every
            // log level. The 2026-04-24 instrumentation sweep missed this.
            return Decision::Escalate(format!(
                "same action repeated {my_count} times \
                 (action_kind={}, action_hash={hash:#018x}, max_repeat={})",
                action.kind(),
                self.max_repeat
            ));
        }
        Decision::Continue
    }

    /// Called when the self-correction sub-loop exhausts its
    /// requery budget without producing a parseable response.
    /// Returns Escalate once `max_parse_failures` consecutive
    /// iterations have done so.
    #[instrument(
        level = "debug",
        skip_all,
        fields(
            consecutive_parse_failures = tracing::field::Empty,
            max_parse_failures = self.max_parse_failures,
        ),
        ret,
    )]
    pub fn record_parse_failure(&mut self) -> Decision {
        self.consecutive_parse_failures += 1;
        tracing::Span::current().record("consecutive_parse_failures", self.consecutive_parse_failures);
        debug!(
            consecutive_parse_failures = self.consecutive_parse_failures,
            max_parse_failures = self.max_parse_failures,
            "lifeguard: parse failure recorded"
        );
        if self.consecutive_parse_failures >= self.max_parse_failures {
            Decision::Escalate(format!(
                "LLM produced unparseable output for {} consecutive iterations (max_parse_failures={})",
                self.consecutive_parse_failures, self.max_parse_failures
            ))
        } else {
            Decision::Continue
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
