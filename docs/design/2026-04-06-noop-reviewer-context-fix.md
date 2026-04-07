# Design Document: NO-OP Bundle Reviewer Context Fix

**Author:** Scott Idler + Claude
**Date:** 2026-04-06
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

The Reviewer agent systematically rejects NO-OP bundles because the ContextBuilder fails to provide file contents for verification. The reviewer prompt correctly instructs "read the provided file contents carefully," but `noop_file_contents` is always `None` because `touched_paths` is empty for noop bundles and `resource_tags` don't map to actual file paths on disk. This is a context assembly bug, not a reviewer logic bug.

## Problem Statement

### Background

The noop bundle pathway (implemented per `2026-04-01-noop-bundle-pathway.md`) added `noop_reason: Option<String>` to the Bundle pipeline. When an Implementer discovers work is already complete, it proposes a noop bundle. The Reviewer is supposed to verify the codebase state against acceptance criteria using provided file contents.

The original design doc's "Resolved Questions" section explicitly states:

> The context builder should prefer `touched_paths` when non-empty, falling back to `resource_tags`.

This was implemented in `src/agents/context.rs:440-458`. However, both fallback layers fail in practice.

### Problem

The ContextBuilder's noop file content assembly has a three-layer failure cascade:

1. **`touched_paths` is always empty for noop bundles.** The executor (`src/agents/executor/action/bundle.rs:138-156`) computes `touched_paths` via `git diff --name-only main...HEAD`, but for noop bundles it explicitly sets `touched_paths` to `vec![]` since there are no changes.

2. **`resource_tags` are abstract, not file paths.** Resource tags are LLM-generated during decomposition (e.g., `"models.py"`, `"app/models.py"`). They represent conceptual file scopes, not verified filesystem paths. In E2E tests, the implementer wrote code into `main.py` and `database.py`, but the resource tags pointed to `app/models.py` which didn't exist.

3. **Silent failure on missing files.** The ContextBuilder uses `std::fs::read_to_string(&full_path)` wrapped in `if let Ok(content)` (line 454), silently dropping paths that don't exist on disk. When every path fails, `noop_file_contents` is `None`.

**Result:** The Reviewer receives the NO-OP directive ("Read the provided file contents carefully") but zero file contents. It tries to follow instructions, finds no evidence, and rejects. The Coordinator reassigns, a new Implementer reaches the same conclusion, and the system enters a doom loop.

**Observed in E2E:** python-api test, Work "Define BookmarkCreate and BookmarkUpdate Pydantic models in models.py" - every noop bundle rejected with "no file contents were provided in the review payload."

### Goals

- Ensure the Reviewer always receives file contents for noop bundles when the repo has source files
- Fix the `touched_paths` gap: populate it from the Implementer's ReadCache (files it actually verified)
- Add a repo-scanning fallback in ContextBuilder when both `touched_paths` and `resource_tags` fail
- Replace silent error swallowing with explicit warnings

### Non-Goals

- Changing the reviewer prompt (it's already correct)
- Changing the noop bundle FSM or IPC protocol
- Making resource_tags more accurate (separate concern - decomposer quality)
- Changing how normal (non-noop) bundles work

## Proposed Solution

### Overview

Two complementary changes:

1. **Executor: populate `touched_paths` from ReadCache on noop bundles.** The Implementer's ReadCache already tracks every file read via `read_file` tool calls. Extract those paths, strip the worktree prefix, and use them as `touched_paths`. This gives the Reviewer targeted context.

2. **ContextBuilder: repo-scan fallback + warn on failures.** When `touched_paths` and `resource_tags` both fail to resolve any readable files, scan the repo for tracked source files. Replace the silent `if let Ok` with explicit `warn!` on each failed path.

### Implementation

#### Change 1: Executor Populates `touched_paths` from ReadCache

**File: `src/agents/executor/action/bundle.rs`, lines 138-156**

Currently, noop bundles set `touched_paths` to `vec![]`. The Implementer's ReadCache (on `AgentContext`) stores every file it read as an absolute path (`worktree_path.join(relative_path)`). We extract those paths, strip the worktree prefix, and deduplicate.

```rust
let touched_paths: Vec<String> = if !is_noop {
    // ... existing git diff logic (unchanged) ...
} else {
    // For noop bundles: extract the files the Implementer actually read
    // during this session. These are the files it inspected to determine
    // the work was already complete.
    let mut seen = std::collections::HashSet::new();
    ctx.cache()
        .unique_paths()
        .filter_map(|abs_path| {
            abs_path
                .strip_prefix(worktree_path)
                .ok()
                .and_then(|rel| rel.to_str())
                .map(String::from)
        })
        .filter(|p| seen.insert(p.clone()))
        .collect()
};
```

**File: `src/agents/cache.rs`**

Add a `unique_paths()` method that returns deduplicated paths (the same file read at different offsets is one path):

```rust
/// Return unique file paths in the cache, deduplicated across offset/limit variants.
pub fn unique_paths(&self) -> impl Iterator<Item = &std::path::Path> + '_ {
    let mut seen = std::collections::HashSet::new();
    self.entries
        .keys()
        .filter_map(move |k| {
            if seen.insert(&k.path) {
                Some(k.path.as_path())
            } else {
                None
            }
        })
}
```

Note: The ReadCache keys are `(PathBuf, Option<u64>, Option<u64>)` - the same file read at offsets 0-500 and 500-1000 creates two entries. `unique_paths()` deduplicates by path.

#### Change 2: ContextBuilder Repo-Scan Fallback + Warnings

**File: `src/agents/context.rs`, lines 440-458**

Replace the silent `if let Ok(content)` with explicit warnings and add a repo-scan fallback:

```rust
if noop_reason.is_some() {
    let repo_path = &self.stores.config.project.repo_path;
    let resource_tags = {
        let works = self.stores.read_works()?;
        works.get(&work_id).map(|w| w.resource_tags.clone()).unwrap_or_default()
    };
    let paths_to_read: Vec<String> = if touched_paths.is_empty() {
        resource_tags
    } else {
        touched_paths
    };

    let mut file_contents = Vec::new();
    for path in &paths_to_read {
        let full_path = repo_path.join(path);
        match std::fs::read_to_string(&full_path) {
            Ok(content) => file_contents.push((path.clone(), content)),
            Err(e) => {
                warn!(
                    "noop context: failed to read '{}': {} (repo={})",
                    path, e, repo_path.display()
                );
            }
        }
    }

    // Fallback: if no files resolved, scan repo for tracked source files
    if file_contents.is_empty() {
        warn!(
            "noop context: no files resolved from touched_paths ({}) or \
             resource_tags, scanning repo for tracked source files",
            paths_to_read.len()
        );
        file_contents = scan_tracked_source_files(repo_path);
    }

    self.noop_file_contents = if file_contents.is_empty() {
        None
    } else {
        Some(file_contents)
    };
}
```

**New helper function in `src/agents/context.rs`:**

```rust
/// Scan the repo for git-tracked source files, returning (relative_path, content) pairs.
/// Used as a last-resort fallback when noop bundle file resolution fails.
/// Bounded by file count and total bytes to stay within context window limits.
fn scan_tracked_source_files(repo_path: &std::path::Path) -> Vec<(String, String)> {
    const MAX_FILES: usize = 20;
    const MAX_TOTAL_BYTES: usize = 32_000; // ~8k tokens

    let output = std::process::Command::new("git")
        .args(["ls-files", "--cached"])
        .current_dir(repo_path)
        .output();

    let tracked = match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return Vec::new(),
    };

    let source_extensions = [
        "py", "rs", "js", "ts", "tsx", "jsx", "go", "java",
        "rb", "lua", "sh", "sql", "html", "css",
    ];

    let skip_prefixes = [
        "node_modules/", "target/", ".git/", "vendor/",
        "dist/", "__pycache__/", ".venv/",
    ];

    let mut results = Vec::new();
    let mut total_bytes: usize = 0;

    for line in tracked.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if skip_prefixes.iter().any(|p| line.starts_with(p)) {
            continue;
        }
        let has_source_ext = std::path::Path::new(line)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|ext| source_extensions.contains(&ext));
        if !has_source_ext {
            continue;
        }

        let full_path = repo_path.join(line);
        if let Ok(content) = std::fs::read_to_string(&full_path) {
            total_bytes += content.len();
            results.push((line.to_string(), content));
            if results.len() >= MAX_FILES || total_bytes >= MAX_TOTAL_BYTES {
                break;
            }
        }
    }
    results
}
```

**Why no config/docs files in the scan:** The `source_extensions` list excludes `.yml`, `.yaml`, `.json`, `.toml`, `.md`, `.txt` because these are typically config or documentation, not the source code the Reviewer needs to verify acceptance criteria against. The Reviewer needs to see the implementation, not the project metadata.

### Data Flow (Fixed)

```
Implementer reads files via read_file tool
  -> ReadCache records absolute paths: /worktrees/wk-xxx/main.py, etc.
Implementer determines work is done
Implementer: ProposeBundle { noop_reason: "already complete" }
  -> executor: touched_paths = ReadCache paths, stripped to relative (main.py, etc.)
  -> bundle.create: touched_paths persisted on Bundle
    -> Reviewer ContextBuilder:
       Layer 1: touched_paths from Bundle -> read from repo_path -> SUCCESS (most cases)
       Layer 2: resource_tags from Work -> read from repo_path -> may succeed
       Layer 3: scan_tracked_source_files(repo_path) -> guaranteed if repo has source
    -> Reviewer: receives file contents, can verify acceptance criteria
```

### Testing Strategy

1. **Unit test - ReadCache `unique_paths()` deduplication:** Record the same path with different offsets, verify `unique_paths()` returns it once. Record different paths, verify all returned.

2. **Unit test - noop `touched_paths` from ReadCache:** In `handle_propose_bundle` with `is_noop=true` and a pre-populated ReadCache, verify the resulting Bundle has non-empty `touched_paths` with correctly stripped relative paths.

3. **Unit test - ContextBuilder resolves noop files from `touched_paths`:** Create a noop bundle with `touched_paths` pointing to real files in a test dir. Build Reviewer context. Verify `noop_file_contents` is populated and user message contains "Current File Contents."

4. **Unit test - ContextBuilder falls back to repo scan:** Create a noop bundle with empty `touched_paths` and invalid `resource_tags`. Place real `.py` source files in a git-init'd test dir. Build Reviewer context. Verify the fallback populates file contents.

5. **Unit test - ContextBuilder warns on failed path resolution:** Create a noop bundle with `resource_tags` pointing to nonexistent files. Capture tracing output. Verify warn-level messages.

6. **Unit test - `scan_tracked_source_files` respects budgets:** Create a git repo with 30+ source files. Verify function returns at most `MAX_FILES` and stays under `MAX_TOTAL_BYTES`.

7. **Unit test - `scan_tracked_source_files` filters correctly:** Create a git repo with `.pyc`, `node_modules/`, `target/` artifacts. Verify they are excluded. Verify `.py` and `.rs` files are included.

## Alternatives Considered

### Alternative 1: Require LLM to Explicitly List `verified_paths` in ProposeBundle

- **Description:** Add a `verified_paths: Vec<String>` field to the `ProposeBundle` action that the LLM must populate when using `noop_reason`.
- **Pros:** Most explicit signal of what the Implementer checked.
- **Cons:** Requires prompt changes, LLM compliance, and a new field on the action. LLMs may forget to populate it or list incorrect paths.
- **Why not chosen:** The ReadCache already has this data implicitly. Extracting from the cache is more reliable than asking the LLM to declare paths.

### Alternative 2: Fuzzy-Match `resource_tags` Against the Filesystem

- **Description:** When `resource_tags` don't resolve as exact paths, try fuzzy matching (e.g., `models.py` matches `app/models.py` or `src/models.py`).
- **Pros:** Could work without any executor changes.
- **Cons:** Ambiguous matches (which `models.py`?), performance cost of recursive search, still fails when resource_tags are conceptual (e.g., `"Pydantic models"`).
- **Why not chosen:** Unreliable and complex. The ReadCache + repo-scan approach is simpler and covers all cases.

### Alternative 3: Always Scan Repo for Noop Reviews (Skip Tiers)

- **Description:** Skip `touched_paths`/`resource_tags` entirely and always scan the repo for noop bundles.
- **Pros:** Simplest implementation.
- **Cons:** Wasteful for large repos. Includes irrelevant files, diluting the Reviewer's focus. Higher token cost.
- **Why not chosen:** The three-tier approach tries precise context first, falling back to broad context only when needed. Better signal-to-noise for the Reviewer.

## Technical Considerations

### Dependencies

- `ReadCache` (exists in `src/agents/cache.rs`) - needs a `unique_paths()` accessor.
- `git ls-files` - used by the repo-scan fallback. Available in any git repo.

### Performance

Negligible. The ReadCache extraction is in-memory. The repo scan runs `git ls-files` (fast, reads index only) and reads at most 20 small source files. Only triggered as a last resort.

### Backward Compatibility

No protocol changes. `touched_paths` already exists on Bundle. The only behavioral change is that noop bundles now populate `touched_paths` from ReadCache instead of `vec![]`. Old persisted bundles with empty `touched_paths` hit the fallback path (repo scan), which didn't exist before but is strictly additive.

### Security

No new attack surface. File reads are bounded to the repo directory and to git-tracked files only (`git ls-files --cached`).

### Path Normalization

The ReadCache stores absolute paths (e.g., `/tmp/.worktrees/wk-xxx/main.py`). The `handle_propose_bundle` function has access to `worktree_path`, so it strips the prefix to get relative paths (e.g., `main.py`). The ContextBuilder then resolves these relative paths against `repo_path` (the main repo). Since noop means no code changes, the files are identical in both locations.

**Edge case:** If the Implementer reads files outside the worktree (e.g., absolute paths to system files), `strip_prefix` returns `None` and those paths are filtered out. This is correct behavior.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| ReadCache is empty (Implementer didn't read files) | Low | Medium | Falls through to resource_tags, then repo scan. Three tiers guarantee context unless the repo is empty. |
| Repo scan includes irrelevant files | Medium | Low | Source extension filter and budget caps constrain the set. Reviewer prompt focuses on acceptance criteria. |
| Large repo exceeds scan budget | Low | Low | `MAX_FILES=20` and `MAX_TOTAL_BYTES=32000` hard limits. Per-file truncation at 4000 chars in `build()`. |
| ReadCache paths don't strip cleanly | Low | Medium | `strip_prefix` returns `None` for paths outside worktree - filtered out. Relative paths pass through unchanged. |

## Open Questions

None - all resolved during review passes.

## References

- `src/agents/context.rs:440-458` - Current noop file content resolution (the bug)
- `src/agents/context.rs:658-680` - Noop file content injection into reviewer prompt
- `src/agents/executor/action/bundle.rs:138-156` - Executor sets `touched_paths` to `vec![]` for noop
- `src/agents/executor/action/file.rs:150-169` - ReadCache `record()` calls with absolute paths
- `src/agents/cache.rs` - ReadCache implementation
- `src/domain/bundle.rs:84-85` - Bundle `noop_reason` field
- `prompts/reviewer.pmt:24-31` - Reviewer noop instructions (already correct)
- `docs/design/2026-04-01-noop-bundle-pathway.md` - Original noop pathway design doc
