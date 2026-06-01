# Implementation Notes: First-Gate Hardening + Failure-Path Tests

Companion to [2026-05-31-gate-hardening.md](2026-05-31-gate-hardening.md).
Append-only. One section per phase, four buckets each.

## Phase F: ScriptedLlm prompt-content keying

### Design decisions
- Keyed match runs against a **haystack = system prompt + all `MessageContent::Text`
  blocks** (for `complete_free`) or **system + user** (for `complete_with_tool`)
  — `crates/llm/src/stub.rs`. Rationale: implementer and reviewer both call
  `complete_free` with `model: None`, so model-routing alone cannot disambiguate
  them; the test author picks a needle substring unique to the (role, Work) pair.
- **Keyed-first, model-FIFO fallback** — a `complete_*` call first scans the keyed
  list for the first entry whose needle is a substring of the haystack; only if
  none matches does it fall back to the existing per-model FIFO queue. Preserves
  every existing caller (they queue no keyed entries, so they hit the FIFO path
  unchanged).
- Keyed store is a `Vec<(String, Result<T, LlmError>)>` (insertion-ordered, first
  match wins on substring), not a `HashMap` — needles are substrings, not exact
  keys, so a map keyed by needle would not help selection.

### Deviations
- None.

### Tradeoffs
- New tests appended to the **existing inline `#[cfg(test)] mod tests`** in
  `stub.rs` rather than extracting to a sibling `stub/tests.rs`. The repo rule
  prefers sibling test files, but its own text says the inline→sibling migration
  is "a tree-wide mechanical pass, never mixed into a feature." Extracting here
  would mix an unrelated refactor into this phase, so the inline module is kept.

### Open questions
- None.
