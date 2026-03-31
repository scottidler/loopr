# Design Document: ReadFile Dedup for Agent Executor

**Author:** Scott Idler + Claude
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Add mtime-based dedup to the agent executor's `ReadFile` action so that when an agent re-reads the same file with the same offset/limit and the file hasn't changed, it gets back "file unchanged" instead of the same truncated content. This breaks the infinite retry loop (Bug #7) at the root cause rather than relying on the 500-line cap to be "big enough."

## Problem Statement

### Background

Bug #7 from the first E2E run: the Implementer entered an infinite retry loop reading `src/tui/input.rs` (1441 lines). The executor returned the full file with no line cap, `format_action_summary()` truncated to 4000 bytes, and the LLM - seeing truncated content - rationally re-issued the same `read_file`. The lifeguard caught it after 5 identical actions and failed the agent.

The immediate fix (already landed) added:
- 500-line default cap with offset/limit parameters on `ReadFile`
- Truncation note: `"... [N more lines, use offset/limit to read specific sections]"`

This helps but doesn't eliminate the loop. Any file >500 lines still produces a truncated read. The LLM sees truncated content and may re-read, especially when it needs to understand the full file before making an edit.

### Problem

The 500-line cap reduces the frequency of the retry loop but doesn't break it. The loop's root cause is that the LLM receives the same truncated content on every re-read and has no signal that "you already have this data, nothing changed." Without that signal, re-reading is rational behavior.

Research into Claude Code's Read tool (v2.1.88) revealed that their primary anti-loop mechanism is **mtime-based dedup**: when the same file is read with the same offset/limit and the file's mtime hasn't changed, Claude Code returns `file_unchanged` instead of re-sending content. The LLM sees a different response, understands it already has the data, and moves on.

### Goals

- **G1**: Eliminate the read-file retry loop for unchanged files
- **G2**: Automatically invalidate the dedup cache when the agent writes or edits the file
- **G3**: Zero overhead for first reads - dedup only activates on re-reads

### Non-Goals

- Token-based size enforcement (Claude Code's second tier) - not needed at current scale
- Byte-size limits - the 500-line cap is sufficient
- Dedup across agents - each agent session is independent
- Dedup for the chat ReadTool (`src/tools/builtin/read.rs`) - chat is human-driven, not a loop

## Proposed Solution

### Overview

Add a per-agent `ReadCache` that tracks `(path, offset, limit) -> mtime` for each file read. On subsequent reads with matching parameters, check the file's current mtime. If unchanged, return a short "file unchanged" message instead of the content. Invalidate cache entries when the agent writes or edits the file.

### Data Model

New file: `src/agents/cache.rs`

```rust
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
struct ReadCacheKey {
    path: PathBuf,
    offset: Option<u64>,
    limit: Option<u64>,
}

/// Per-read metadata stored in the cache.
#[derive(Debug, Clone)]
struct ReadCacheEntry {
    mtime: SystemTime,
    total_lines: usize,
}

/// Tracks file reads within a single agent session to detect unchanged re-reads.
///
/// Two-phase API: call `check_hit` before reading the file. On a hit, skip the
/// read entirely. On a miss, read the file, then call `record` to populate the
/// cache for next time.
#[derive(Debug, Default)]
pub struct ReadCache {
    entries: HashMap<ReadCacheKey, ReadCacheEntry>,
}

impl ReadCache {
    /// Check whether a read is a dedup hit (same path + offset + limit, mtime
    /// unchanged). Returns Some(total_lines) on hit, None on miss.
    /// Does NOT insert on miss - call `record` after reading the file.
    pub fn check_hit(
        &self,
        path: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
        current_mtime: SystemTime,
    ) -> Option<usize> {
        let key = ReadCacheKey {
            path: path.to_path_buf(),
            offset,
            limit,
        };
        match self.entries.get(&key) {
            Some(entry) if entry.mtime == current_mtime => {
                Some(entry.total_lines)
            }
            _ => None,
        }
    }

    /// Record a completed read so future identical reads can be deduped.
    pub fn record(
        &mut self,
        path: &Path,
        offset: Option<u64>,
        limit: Option<u64>,
        mtime: SystemTime,
        total_lines: usize,
    ) {
        let key = ReadCacheKey {
            path: path.to_path_buf(),
            offset,
            limit,
        };
        self.entries.insert(key, ReadCacheEntry { mtime, total_lines });
    }

    /// Invalidate all cache entries for a path (any offset/limit).
    /// Called after write_file, edit_file, or delete actions.
    pub fn invalidate(&mut self, path: &Path) {
        self.entries.retain(|k, _| k.path != *path);
    }
}
```

### Integration Points

**1. Ownership: `ReadCache` on `AgentContext`**

`execute_action` takes `&AgentContext` (immutable reference). Three callers: `implementer.rs:325`, `researcher.rs:334`, `coordinator.rs:1486`, plus ~60 test call sites. Changing the signature to take `&mut ReadCache` would require modifying all of them.

Instead, add `ReadCache` to `AgentContext` behind `std::sync::Mutex`:

```rust
// src/agents/mod.rs - AgentContext
pub struct AgentContext {
    pub session: AgentSession,
    pub stores: Arc<Stores>,
    pub bridge: AgentIpcBridge,
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub tool_runner: Arc<ToolRunner>,
    pub tool_executor: Arc<ToolExecutor>,
    pub log: AgentLogger,
    pub read_cache: std::sync::Mutex<ReadCache>,  // new
}
```

`std::sync::Mutex` (not `tokio::sync::Mutex`) because the lock is never held across an `.await` point - it's acquired, checked/inserted, and released within a single synchronous block. This is cheaper and `Send`-safe.

The `AgentContext::from_session_id` constructor initializes it as `Mutex::new(ReadCache::default())`. Since `AgentContext` is created per agent session, the cache naturally scopes to one agent's lifetime.

Existing call sites don't change - they pass `&ctx` as before. Only the `ReadFile`, `WriteFile`, and `EditFile` match arms inside `execute_action` touch the cache.

**2. ReadFile handler (`src/agents/executor.rs:557`)**

```rust
AgentAction::ReadFile { path, offset, limit } => {
    let full_path = validate_sandboxed_path(worktree_path, path, false)?;

    // Stat for mtime (single syscall, kernel-cached)
    let mtime = tokio::fs::metadata(&full_path).await
        .map_err(|e| eyre!("read_file '{}': {}", path, e))?
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH);

    // Dedup check BEFORE reading the file
    if let Some(cached_lines) = ctx.read_cache.lock()
        .expect("read_cache poisoned")
        .check_hit(&full_path, *offset, *limit, mtime)
    {
        let start = offset.unwrap_or(1).max(1);
        let effective_limit = limit.unwrap_or(500);
        let end = (start + effective_limit - 1).min(cached_lines as u64);
        return Ok(ActionResult::FileRead(format!(
            "File unchanged since last read \
             (lines {}-{} of {}, use offset/limit for other sections, \
             or proceed with editing).",
            start, end, cached_lines
        )));
    }

    // Cache miss - read the file
    let content = tokio::fs::read_to_string(&full_path)
        .await
        .map_err(|e| eyre!("read_file '{}': {}", path, e))?;
    let lines: Vec<&str> = content.lines().collect();

    // Record in cache for future dedup
    ctx.read_cache.lock()
        .expect("read_cache poisoned")
        .record(&full_path, *offset, *limit, mtime, lines.len());

    // Normal read path (existing line cap, numbering, truncation note)
    // ... existing code unchanged ...
}
```

On dedup hits, only one `metadata()` syscall + one Mutex lock + one HashMap lookup. No `read_to_string`, no line processing, no formatting. The total_lines value comes from the cache entry recorded during the first read.

**3. Write/Edit invalidation**

After `WriteFile` and `EditFile` complete successfully, invalidate:

```rust
AgentAction::WriteFile { path, content } => {
    // ... existing write logic ...
    ctx.read_cache.lock()
        .expect("read_cache poisoned")
        .invalidate(&full_path);
    Ok(ActionResult::FileWritten(path.clone()))
}

AgentAction::EditFile { path, old_string, new_string } => {
    // ... existing edit logic ...
    ctx.read_cache.lock()
        .expect("read_cache poisoned")
        .invalidate(&full_path);
    Ok(ActionResult::FileEdited(path.clone()))
}
```

**4. ReadCache lifetime**

The `ReadCache` is created when `AgentContext` is built (per agent session) and dropped when the context is dropped. It does not persist across agent restarts. This is correct - a restarted agent has a fresh context window and should re-read files.

### Interaction with Existing Mechanisms

| Mechanism | Role | Interaction with Dedup |
|-----------|------|----------------------|
| 500-line cap | Limits content size per read | Unchanged. On dedup hits, file read is skipped entirely - the LLM gets "unchanged" instead of numbered content |
| Truncation note | Tells LLM about remaining lines | Unchanged. Only shown on first read (dedup prevents re-reads from reaching this path) |
| format_action_summary | Truncates action results to 4000 bytes | Dedup responses are short (~80 bytes), well within the budget |
| Lifeguard | Catches 5 consecutive identical actions | Still fires as a backstop. The lifeguard hashes the *action* (input), not the *result* (output). So 5 identical `read_file` actions still trigger escalation. But dedup changes the *result*, which changes the LLM's next action, so the lifeguard counter resets naturally. If the LLM somehow ignores the "unchanged" message and re-reads 5 times, the lifeguard correctly escalates. |
| offset/limit | Pagination for large files | Dedup is per (path, offset, limit) tuple. Reading offset=1 then offset=500 are distinct entries. Re-reading offset=1 with same mtime is a dedup hit |

### Prompt Update

Update `prompts/implementer.pmt` to mention the dedup behavior:

```
3. `read_file` - Read a file from the worktree (default: first 500 lines). Use `"offset": N, "limit": M` for large files. Re-reading an unchanged file returns "file unchanged" with the line range you already have - use that to target offset/limit for other sections, or proceed to editing.
```

## Alternatives Considered

### Alternative 1: Increase the line cap to 2000 (matching Claude Code)
- **Description:** Bump the default cap from 500 to 2000 lines, making truncation rarer.
- **Pros:** Simple one-line change. Most project files fit in 2000 lines.
- **Cons:** Doesn't fix the loop for files >2000 lines. Wastes context budget - Loopr's implementer agents have much smaller context than Claude Code's opus[1m]. 2000 lines of source at ~10 tokens/line is 20k tokens, a large fraction of an agent's iteration budget.
- **Why not chosen:** Treats the symptom (truncation frequency) not the cause (no signal that content is unchanged).

### Alternative 2: Content-hash dedup instead of mtime
- **Description:** Hash the file content and compare hashes instead of mtimes.
- **Pros:** Immune to filesystem mtime quirks (copy operations, network filesystems).
- **Cons:** Requires reading the entire file to compute the hash, defeating the "skip the read" optimization. Adds CPU cost for large files.
- **Why not chosen:** mtime is free (single stat syscall) and sufficient for local worktrees where files only change via agent writes.

### Alternative 3: Suppress re-reads entirely via lifeguard (lower threshold)
- **Description:** Reduce the lifeguard's action_threshold from 5 to 2 for ReadFile actions.
- **Pros:** No new code. Already works.
- **Cons:** Fails the agent on the second identical read, which is too aggressive. Sometimes the LLM legitimately re-reads a file after editing it (to verify the edit). The lifeguard is a blunt instrument - it can't distinguish "unchanged re-read" from "post-edit verification read."
- **Why not chosen:** Dedup handles the distinction correctly - post-edit reads see a new mtime and return content normally.

### Alternative 4: Track reads in the LLM conversation context
- **Description:** Include a note in the system prompt: "You have already read these files: [list]"
- **Pros:** LLM-native solution, no executor changes.
- **Cons:** LLMs don't reliably use system prompt metadata to suppress their own actions. The content is still in the context window but may be far back. Adding another system prompt section increases prompt complexity.
- **Why not chosen:** LLM-side hints are unreliable. Executor-side dedup is deterministic.

## Technical Considerations

### Performance

- **First reads**: One additional `metadata()` syscall (kernel-cached) + one Mutex lock/unlock + one HashMap miss before the existing `read_to_string()`. Negligible overhead.
- **Dedup hits**: One `metadata()` syscall + one Mutex lock + one HashMap hit. Skips `read_to_string()`, line splitting, numbering, and formatting entirely. LLM receives ~80 bytes instead of ~20KB. Savings in both I/O and context budget.
- **Memory**: One `ReadCacheKey` + `ReadCacheEntry` (~96 bytes) per unique (path, offset, limit) read. An agent reading 50 files stores ~5KB. Negligible.
- **Lock contention**: `std::sync::Mutex` held for <1us per operation (HashMap lookup/insert). Agents are single-threaded per session, so there is no contention. Two lock acquisitions per cache-miss read (one for check, one for record) is fine.

### Edge Cases

| Case | Behavior |
|------|----------|
| File deleted between reads | `metadata()` returns ENOENT, executor returns normal error. No dedup interaction. |
| File modified externally (e.g., by another process in the worktree) | mtime changes, dedup misses, full content returned. Correct behavior. |
| Agent writes then re-reads | `invalidate()` clears the entry, re-read returns fresh content. Correct. |
| Agent reads with offset=1,limit=500, then offset=1,limit=100 | Different cache keys, no dedup. Correct - different content requested. |
| Agent reads with no offset/limit, then with explicit offset=1,limit=500 | Different cache keys (None vs Some(1), None vs Some(500)). No dedup. Slightly wasteful but correct and rare. |
| Filesystem with 1-second mtime granularity | If a write and re-read happen within the same second, mtime may not change. The `invalidate()` call on write prevents this - the cache entry is removed regardless of mtime. |
| Mutex poisoning | A panic inside a lock guard poisons the mutex. Use `.expect("read_cache poisoned")` - this is acceptable per project conventions since `.expect("reason")` is allowed in production when the reason is clear. A poisoned mutex means a prior panic, so propagating is correct. |
| Filesystem without mtime support | `.modified()` falls back to `UNIX_EPOCH`. Since the path is part of the key, different files won't collide. But for the same file, every second read would hit the cache even if content changed externally. Mitigated by `invalidate()` on agent writes. Not a real risk - Loopr operates on local Linux worktrees (ext4/btrfs/xfs) which all support mtime. |
| LLM varies parameters to circumvent dedup | If the LLM reads with no offset, then with explicit offset=1, these are different cache keys - no dedup, no lifeguard catch. In practice LLMs don't deliberately vary parameters to re-read the same content. If this becomes a pattern, normalize None to defaults before keying. |

### Testing Strategy

1. **Unit tests for ReadCache** (in `src/agents/cache.rs`):
   - `check_hit` returns `None` before any `record` call
   - After `record`, `check_hit` returns `Some(total_lines)` with same mtime
   - `check_hit` returns `None` when mtime differs (file was modified)
   - `invalidate` clears all entries for a path regardless of offset/limit
   - Different offset/limit = different cache keys (no false dedup)
   - `record` then `invalidate` then `check_hit` = miss (invalidation works)

2. **Integration tests for executor** (in `src/agents/executor.rs` test module):
   - Read a file twice with same path/offset/limit: second returns "File unchanged"
   - Write to the file, read again: returns fresh content (invalidation worked)
   - Edit the file, read again: returns fresh content
   - Read with offset=None, then offset=Some(1): no dedup (different keys)

3. **Existing tests**: All ~60 existing `execute_action` tests continue to pass unchanged. They create their own `AgentContext` (which now includes `Mutex::new(ReadCache::default())`), and since they only read each file once, dedup never activates.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| mtime granularity causes false dedup hit after rapid write+read | Low | Medium | `invalidate()` is called on every write/edit, clearing the entry regardless of mtime. The race only occurs if an external process modifies the file in the same second without going through the executor. |
| LLM confused by "file unchanged" message | Low | Low | Message includes total line count and suggests "proceed with editing." Prompt update reinforces this. |
| Cache grows unbounded for agents reading many files | Very Low | Very Low | Agents have a 20-iteration cap. Even reading a new file every iteration is 20 entries (~1.6KB). |

## Open Questions

- [x] Should the dedup message include the line range that was previously read? **Yes** - resolved in Pass 5. The message now shows "lines 1-500 of 1441" which helps the LLM remember what it has and which sections to target with offset/limit.
- [ ] Should the chat ReadTool (`src/tools/builtin/read.rs`) also get dedup? It already has `ctx.track_read()` which could be extended. Lower priority since chat is human-driven.

## References

- First E2E run design: `docs/design/2026-03-30-first-end-to-end-run.md` (Bug #7 description)
- Claude Code Read tool analysis: research from previous conversation thread
- Native tool use: `docs/design/2026-03-04-native-tool-use.md`
- Orchestration spine (agent executor): `docs/design/2026-02-25-orchestration-spine.md`
