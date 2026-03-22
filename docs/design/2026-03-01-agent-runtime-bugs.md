# Design Document: Agent Runtime Bug Fixes

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Manual end-to-end testing of the Implementer agent revealed four bugs that make the agent loop unable to create files, parse flexible LLM output, or detect futile loops. This document proposes fixes for all four bugs: path sandboxing for non-existent files, ReadFile path validation, flexible `RunTool.args` deserialization, and a multi-tier circuit breaker.

## Problem Statement

### Background

The Agent struct refactoring (commits `57e4c23..f5e6ecf`) consolidated agent logic into struct-based `run()` methods. The refactoring preserved all tests but did not exercise the full runtime path. A manual test — spawning an Implementer to build a Python todo app — exposed four bugs that together render the Implementer non-functional.

### Problem

**Bug 1 (Critical): `WriteFile` rejects all new files with "path escapes worktree"**

In `executor.rs:386`, `full_path.canonicalize()` fails for files that don't exist yet. The fallback returns the raw joined path (relative if `worktree_path` is relative). Meanwhile `worktree_path.canonicalize()` succeeds and returns an absolute path. `starts_with` comparing relative vs absolute always fails.

```rust
// Current code (executor.rs:385-391)
let full_path = worktree_path.join(path);
let canonical = full_path.canonicalize().unwrap_or_else(|_| full_path.clone());
let worktree_canonical = worktree_path.canonicalize().unwrap_or_else(|_| worktree_path.to_path_buf());
if !canonical.starts_with(&worktree_canonical) {
    return Err(eyre!("path escapes worktree: {}", path));  // Always hits for new files
}
```

**Bug 2 (Minor): `ReadFile` lacks path sandboxing**

`ReadFile` handler (`executor.rs:421-427`) resolves the path relative to the worktree but performs no containment check. An LLM could request `read_file("../../.env")` and exfiltrate secrets.

**Bug 3 (Minor): `RunTool.args` rejects string values**

`AgentAction::RunTool.args` is typed as `Vec<String>`. When the LLM returns `"args": "--collect-only"` (a string) instead of `"args": ["--collect-only"]` (an array), `serde_json::from_str` fails and the iteration is wasted on a parse error.

**Bug 4 (Observation): No circuit breaker for futile loops**

The agent burned 10 of 15 iterations retrying the same failing `WriteFile` action. There is no mechanism to detect consecutive identical errors, repeated identical actions, or A-B-A-B oscillation patterns. The only protection is the hard `max_iterations` cap.

### Goals

- Fix all four bugs with minimal code change
- Extract reusable path validation into a shared module
- Add flexible deserialization for LLM-facing action fields
- Implement a multi-tier circuit breaker following industry patterns
- Maintain existing test coverage (currently 93.8%)

### Non-Goals

- OS-level sandboxing (mount namespaces, Seatbelt profiles) — out of scope for MVP4
- LLM-based loop detection (Gemini CLI's Tier 3) — complex, defer to MVP5
- Full action requery pipeline (SWE-agent style re-prompting) — defer to MVP5
- Windows path support (`dunce` crate) — Linux-only for now

## Proposed Solution

### Overview

Four changes, loosely coupled:

1. **`path_sandbox` module** — shared path validation for `WriteFile`, `ReadFile`, and Researcher
2. **Serde coercion** — `string_or_vec` custom deserializer on LLM-facing `Vec<String>` fields
3. **`CircuitBreaker` struct** — per-agent-session loop detection with escalation
4. **Researcher alignment** — migrate Researcher's bespoke path checks to the shared module

### Architecture

```
src/agents/
├── path_sandbox.rs    ← NEW: validate_sandboxed_path()
├── circuit_breaker.rs ← NEW: CircuitBreaker struct
├── executor.rs        ← MODIFIED: use path_sandbox for WriteFile/ReadFile
├── researcher.rs      ← MODIFIED: use path_sandbox instead of inline checks
├── implementer.rs     ← MODIFIED: wire CircuitBreaker into run loop
├── coordinator.rs     ← MODIFIED: wire CircuitBreaker into run loop
├── reviewer.rs        ← MODIFIED: wire CircuitBreaker into run loop
└── mod.rs             ← MODIFIED: string_or_vec on AgentAction fields
```

### Implementation: Bug 1 + Bug 2 — Path Sandbox Module

**New file: `src/agents/path_sandbox.rs`**

Three-layer defense, informed by Docker's `FollowSymlinkInScope`, Go's `filepath-securejoin`, and Anthropic's sandbox-runtime CVE post-mortem:

```rust
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use eyre::{Result, eyre};

/// Validate that `relative` resolves within `root`, even if the file doesn't exist.
///
/// Layer 1 (lexical): reject absolute paths and `..` components — no I/O.
/// Layer 2 (filesystem): canonicalize deepest existing ancestor, append tail,
///   verify containment via `starts_with` against canonicalized root.
/// Layer 3 (denylist): block sensitive file patterns (optional).
pub fn validate_sandboxed_path(root: &Path, relative: &str, check_denylist: bool) -> Result<PathBuf> {
    let rel_path = Path::new(relative);

    // Layer 1a: reject absolute paths
    if rel_path.is_absolute() {
        return Err(eyre!("absolute paths not allowed: {}", relative));
    }

    // Layer 1b: reject `..` components
    for component in rel_path.components() {
        if component == Component::ParentDir {
            return Err(eyre!("path traversal not allowed: {}", relative));
        }
    }

    let full = root.join(relative);

    // Layer 2: canonicalize deepest existing ancestor + append remainder
    let canonical = canonicalize_nonexistent(&full);
    let root_canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    if !canonical.starts_with(&root_canonical) {
        return Err(eyre!("path escapes sandbox: {}", relative));
    }

    // Layer 3: denylist
    if check_denylist {
        check_denylist_path(&full, relative)?;
    }

    Ok(full)
}

/// Canonicalize a path that may not exist by walking up to the deepest existing
/// ancestor, canonicalizing it, then appending the non-existent tail.
fn canonicalize_nonexistent(path: &Path) -> PathBuf {
    if path.exists() {
        return path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    }

    let mut ancestor = path.to_path_buf();
    let mut tail_parts: Vec<OsString> = Vec::new();

    loop {
        if let Some(name) = ancestor.file_name() {
            tail_parts.push(name.to_os_string());
        } else {
            break;
        }
        if !ancestor.pop() {
            break;
        }
        if ancestor.exists() {
            break;
        }
    }

    tail_parts.reverse();
    let canonical_ancestor = ancestor.canonicalize().unwrap_or(ancestor);
    canonical_ancestor.join(tail_parts.iter().collect::<PathBuf>())
}
```

**Changes to `executor.rs`:**

Replace the inline path check in `WriteFile` with:

```rust
AgentAction::WriteFile { path, content } => {
    let full_path = path_sandbox::validate_sandboxed_path(worktree_path, path, false)?;
    // ... rest of write logic unchanged
}
```

Add path validation to `ReadFile`:

```rust
AgentAction::ReadFile { path } => {
    let full_path = path_sandbox::validate_sandboxed_path(worktree_path, path, false)?;
    let content = tokio::fs::read_to_string(&full_path).await?;
    Ok(ActionResult::FileRead(content))
}
```

**Changes to `researcher.rs`:**

Replace inline `validate_path()` (lines 15-80) with calls to `path_sandbox::validate_sandboxed_path(repo_root, path, true)` (denylist enabled). The current `validate_path` takes an `AgentLogger` param for debug logging — the caller should log before the call instead. The denylist constants (`PATH_DENYLIST`, `EXT_DENYLIST`) move into `path_sandbox.rs`. Delete the now-redundant `validate_path` function from `researcher.rs`.

### Implementation: Bug 3 — Flexible Serde Deserialization

**Custom deserializer in `src/agents/mod.rs`:**

```rust
/// Deserialize a JSON value that is either a single string or an array of strings
/// into a Vec<String>. Handles LLM deviations where a string is sent instead of an array.
fn string_or_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct StringOrVecVisitor;

    impl<'de> de::Visitor<'de> for StringOrVecVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or array of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
            Ok(vec![v.to_owned()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Self::Value, A::Error> {
            serde::Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
        }

        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok(Vec::new())  // handles JSON `null`
        }
    }

    deserializer.deserialize_any(StringOrVecVisitor)
}
```

**Apply to all LLM-facing `Vec<String>` fields in `AgentAction`:**

```rust
RunTool {
    tool: String,
    #[serde(default, deserialize_with = "string_or_vec")]
    args: Vec<String>,
},
Commit {
    message: String,
    #[serde(default, deserialize_with = "string_or_vec")]
    paths: Vec<String>,
},
ProposeBundle {
    #[serde(default)]
    description: String,
    #[serde(default, deserialize_with = "string_or_vec")]
    claims: Vec<String>,
},
CreateWork {
    // ...
    #[serde(default, deserialize_with = "string_or_vec")]
    resource_tags: Vec<String>,
    #[serde(default, deserialize_with = "string_or_vec")]
    acceptance_criteria: Vec<String>,
    #[serde(default, deserialize_with = "string_or_vec")]
    dependencies: Vec<String>,
},
```

### Implementation: Bug 4 — Circuit Breaker

**New file: `src/agents/circuit_breaker.rs`**

Informed by Gemini CLI's `LoopDetectionService` (hash-based tool call detection, threshold=5), SWE-agent's `max_requeries` + consecutive timeout tracking, and the multi-agent failure mode research.

```rust
use std::collections::hash_map::DefaultHasher;
use std::collections::VecDeque;
use std::hash::{Hash, Hasher};

/// Decision from the circuit breaker after checking an action or error.
#[derive(Debug, Clone, PartialEq)]
pub enum CircuitBreakerDecision {
    /// Continue normally.
    Continue,
    /// Escalate to NeedHelp — the agent is stuck.
    Escalate(String),
}

/// Per-session circuit breaker. Tracks repeated actions and errors
/// to detect futile loops before the hard iteration cap.
pub struct CircuitBreaker {
    /// Consecutive identical action hashes.
    last_action_hash: Option<u64>,
    consecutive_action_count: u32,
    action_threshold: u32,

    /// Recent error hashes (sliding window).
    recent_errors: VecDeque<u64>,
    error_window_size: usize,
    error_threshold: u32,

    /// Consecutive parse failures.
    consecutive_parse_failures: u32,
    max_parse_retries: u32,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            last_action_hash: None,
            consecutive_action_count: 0,
            action_threshold: 5,          // Gemini CLI: 5 identical tool calls
            recent_errors: VecDeque::new(),
            error_window_size: 10,        // last 10 errors
            error_threshold: 3,           // same error 3x in window
            consecutive_parse_failures: 0,
            max_parse_retries: 3,         // SWE-agent: 3 format retries
        }
    }

    /// Check whether a sequence of actions indicates a loop.
    /// Call once per action before execution.
    pub fn check_action(&mut self, action_hash: u64) -> CircuitBreakerDecision {
        if self.last_action_hash == Some(action_hash) {
            self.consecutive_action_count += 1;
        } else {
            self.last_action_hash = Some(action_hash);
            self.consecutive_action_count = 1;
        }

        if self.consecutive_action_count >= self.action_threshold {
            return CircuitBreakerDecision::Escalate(format!(
                "Repeated identical action {} consecutive times",
                self.consecutive_action_count
            ));
        }

        CircuitBreakerDecision::Continue
    }

    /// Record an action error and check for repeated identical errors.
    pub fn record_error(&mut self, error: &str) -> CircuitBreakerDecision {
        let hash = hash_string(error);

        self.recent_errors.push_back(hash);
        if self.recent_errors.len() > self.error_window_size {
            self.recent_errors.pop_front();
        }

        let same_count = self.recent_errors.iter().filter(|h| **h == hash).count() as u32;
        if same_count >= self.error_threshold {
            return CircuitBreakerDecision::Escalate(format!(
                "Same error repeated {} times: {}",
                same_count,
                truncate(error, 200),
            ));
        }

        CircuitBreakerDecision::Continue
    }

    /// Record a parse failure. Returns Escalate if max retries exceeded.
    pub fn record_parse_failure(&mut self) -> CircuitBreakerDecision {
        self.consecutive_parse_failures += 1;
        if self.consecutive_parse_failures > self.max_parse_retries {
            return CircuitBreakerDecision::Escalate(format!(
                "Failed to produce valid output after {} parse retries",
                self.max_parse_retries
            ));
        }
        CircuitBreakerDecision::Continue
    }

    /// Reset parse failure counter after a successful parse.
    pub fn reset_parse_failures(&mut self) {
        self.consecutive_parse_failures = 0;
    }
}

fn hash_string(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}
```

**Wiring into agent loops:**

**Action hashing:** `AgentAction` derives `Serialize`, so we hash the JSON representation:

```rust
pub fn hash_action(action: &AgentAction) -> u64 {
    let json = serde_json::to_string(action).unwrap_or_default();
    hash_string(&json)
}
```

Each agent's `run()` method creates a `CircuitBreaker` and checks it:

```rust
// In ImplementerAgent::run():
let mut breaker = CircuitBreaker::new();

for i in 1..=self.config.max_iterations {
    // ... build prompt, call LLM ...

    match parse_actions(&response, &self.ctx.log) {
        Ok(actions) => {
            breaker.reset_parse_failures();
            for action in &actions {
                let action_hash = hash_action(action);
                if let CircuitBreakerDecision::Escalate(reason) = breaker.check_action(action_hash) {
                    self.ctx.warn(&format!("circuit breaker: {}", reason));
                    // Emit NeedHelp and return
                    return Ok(());
                }
                match execute_action(action, &self.ctx, &self.worktree_path, Some(&self.work_id)).await {
                    Ok(r) => r,
                    Err(e) => {
                        let err_msg = e.to_string();
                        if let CircuitBreakerDecision::Escalate(reason) = breaker.record_error(&err_msg) {
                            self.ctx.warn(&format!("circuit breaker: {}", reason));
                            return Ok(());
                        }
                        ActionResult::ActionError(err_msg)
                    }
                };
            }
        }
        Err(e) => {
            if let CircuitBreakerDecision::Escalate(reason) = breaker.record_parse_failure() {
                self.ctx.warn(&format!("circuit breaker: {}", reason));
                return Ok(());
            }
            // ... existing retry logic ...
        }
    }
}
```

### Data Model

No schema changes. `CircuitBreaker` is transient (per-session, in-memory only). `AgentAction` serde changes are backward-compatible (existing arrays still deserialize correctly).

### Implementation Plan

| Phase | What | Files |
|-------|------|-------|
| 1 | `path_sandbox.rs` module + tests | `path_sandbox.rs` (new), `mod.rs` |
| 2 | Wire path sandbox into `executor.rs` WriteFile + ReadFile | `executor.rs` |
| 3 | Migrate Researcher to shared path sandbox | `researcher.rs` |
| 4 | `string_or_vec` deserializer + tests | `mod.rs` |
| 5 | `circuit_breaker.rs` module + tests | `circuit_breaker.rs` (new), `mod.rs` |
| 6 | Wire circuit breaker into Implementer, Coordinator, Reviewer, Researcher | `implementer.rs`, `coordinator.rs`, `reviewer.rs`, `researcher.rs` |
| 7 | Integration test: re-run the todo app manual test | manual |

## Alternatives Considered

### Alternative 1: `soft-canonicalize` crate
- **Description:** Use the `soft-canonicalize` crate with its `anchored` feature for path containment.
- **Pros:** Zero custom code for path resolution. Well-tested.
- **Cons:** New dependency for a ~30-line function. The `anchored` feature clamps (silently rewrites) escape paths instead of rejecting them — we want explicit errors.
- **Why not chosen:** Hand-rolled `canonicalize_nonexistent` is small, has no dependencies, and gives us explicit error messages.

### Alternative 2: `libpathrs` (fd-based containment)
- **Description:** Use kernel-level `openat2(RESOLVE_BENEATH)` for TOCTOU-safe path resolution.
- **Pros:** Gold standard for container runtimes. Immune to symlink races.
- **Cons:** Linux 5.6+ only. Overkill for our threat model (LLM-generated paths, not active attackers). Heavy dependency.
- **Why not chosen:** Threat model doesn't justify the complexity.

### Alternative 3: LLM-based loop detection (Gemini CLI Tier 3)
- **Description:** After N turns, send conversation history to a fast model to detect "unproductive state."
- **Pros:** Catches subtle patterns that hash-based detection misses (cognitive loops, oscillation with slight variation).
- **Cons:** Adds LLM cost per session. Requires careful prompt engineering. Latency per check.
- **Why not chosen:** Deferred to MVP5. The hash-based Tier 1-2 detection catches the immediate problem (identical WriteFile failures). LLM-based detection is an enhancement, not a prerequisite.

### Alternative 4: `serde_with::OneOrMany`
- **Description:** Use the `serde_with` crate's `OneOrMany` adapter instead of a custom deserializer.
- **Pros:** Well-tested, single attribute annotation.
- **Cons:** Pulls in `serde_with` (large dependency tree) for one feature. Custom `string_or_vec` is 20 lines and zero-dep.
- **Why not chosen:** Prefer minimal dependencies per project conventions.

## Technical Considerations

### Dependencies

No new external crates. All changes use `std` and existing dependencies (`serde`, `eyre`, `tokio`).

### Performance

- `canonicalize_nonexistent`: One `exists()` check per ancestor directory (typically 1-3 levels). Negligible compared to LLM latency.
- `CircuitBreaker`: O(1) hash comparison per action, O(n) scan of error window (n≤10). Negligible.
- `string_or_vec`: Zero overhead when input is already an array (the `visit_seq` path is identical to default).

### Security

- Bug 2 fix (ReadFile sandbox) closes a path traversal vector for data exfiltration.
- Denylist on Researcher prevents reading `.env`, credentials, and key files.
- Three-layer defense (lexical + filesystem + denylist) follows Docker/runc patterns.
- No TOCTOU risk in our threat model (LLM generates paths, no concurrent attacker).

### Testing Strategy

Each module gets unit tests following the existing pattern:

**`path_sandbox.rs` tests (~12):**
- Rejects absolute paths, `..` traversal, paths escaping root
- Allows new files within root, nested non-existent directories
- Handles symlinks in existing ancestor directories
- Denylist blocks `.env`, `.key`, `.pem`, `credentials.*`

**`circuit_breaker.rs` tests (~10):**
- Consecutive identical actions below/at/above threshold
- Error deduplication within window
- Parse failure counting and reset
- Different actions reset the consecutive counter
- Window eviction

**`mod.rs` serde tests (~5):**
- `string_or_vec`: string input, array input, missing field, empty array, null

**`executor.rs` integration tests:**
- Existing `WriteFile`/`ReadFile` tests updated to use new path validation
- New test: write to non-existent file in worktree succeeds
- New test: write with `../` in path rejected

### Rollout Plan

Single PR to `v3` branch. All changes are backward-compatible. The `string_or_vec` deserializer accepts both old (array) and new (string) formats. No migration needed.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `canonicalize_nonexistent` has edge case on deeply nested symlinks | Low | Med | Layer 1 (`..` rejection) blocks the most dangerous cases. Layer 2 is defense-in-depth. |
| Circuit breaker false positives (legitimate retries flagged as loops) | Low | Med | Threshold of 5 consecutive identical actions is generous. Error window of 10 prevents transient failures from triggering. |
| `string_or_vec` breaks existing serialization round-trips | Low | Low | Only changes deserialization. Serialization is untouched. Existing tests validate round-trips. |
| Researcher migration introduces regressions | Med | Low | Researcher's existing denylist tests are preserved. `validate_sandboxed_path` is a strict superset of the current inline checks. |

## Open Questions

- [ ] Should `CircuitBreaker` thresholds be configurable per-agent-type in `loopr.yml`?
- [ ] Should the circuit breaker emit a `NeedHelp` action or directly transition the agent to `Failed`?
- [ ] Should `ReadFile` use the denylist (blocking reads of `.env` etc.) for all agents or only Researcher?

## References

- [Anthropic Engineering: Claude Code Sandboxing](https://www.anthropic.com/engineering/claude-code-sandboxing) — OS-level sandbox architecture, prefix-matching CVE
- [CVE-2025-59536](https://research.checkpoint.com/2026/rce-and-api-token-exfiltration-through-claude-code-project-files-cve-2025-59536/) — `starts_with` prefix bypass in Claude Code
- [cyphar/filepath-securejoin](https://github.com/cyphar/filepath-securejoin) — Go path containment for runc/Kubernetes
- [Gemini CLI LoopDetectionService](https://github.com/google-gemini/gemini-cli/blob/main/packages/core/src/services/loopDetectionService.ts) — Three-tier loop detection (hash, content chanting, LLM)
- [SWE-agent agents.py](https://github.com/SWE-agent/SWE-agent/blob/main/sweagent/agent/agents.py) — `max_requeries=3`, consecutive timeout tracking, error template system
- [Why Do Multi-Agent LLM Systems Fail?](https://arxiv.org/html/2503.13657v1) — 14 failure modes, "context exhaustion" pattern
- Serde string-or-struct pattern: [serde.rs/string-or-struct](https://serde.rs/string-or-struct.html)
- Loopr MVP4 design doc: `docs/design/2026-02-26-multi-level-rwl.md`
