# Design Document: Scoped Git Staging and Loose File Reporting

**Author:** Scott A. Idler
**Date:** 2026-04-02
**Status:** Implemented
**Review Passes Completed:** 5/5 + Architect Review

## Summary

Replace the unconditional `git add -A` in the bundle proposal and commit paths with resource_tags-aware scoped staging. Files modified but excluded from the commit are reported as `loose_files` on the Bundle, creating an observable signal for downstream agents (Reviewer, Coordinator) to detect scope gaps, widen resource_tags, or create new Work items.

## Problem Statement

### Background

Loopr's orchestration model assigns each Work a set of `resource_tags` - file paths declaring the work's scope. These tags serve three purposes today:

1. **Scheduling** - `work_queue.rs:74-82` avoids scheduling Works with contending resource_tags concurrently via advisory locks
2. **Prompt guidance** - `implementer.pmt:58` tells the LLM "Do not modify files outside these paths"
3. **Reviewer context** - `context.rs:415-420` reads resource_tags files to show the Reviewer for noop bundles

However, resource_tags are never programmatically enforced at the git staging, commit, or bundle creation boundaries.

### Problem

Two code paths blindly stage all files in the worktree:

**Path 1 - Explicit commit action** (`file.rs:189-191`):
```rust
let add_args = if paths.is_empty() { vec!["-A".to_string()] } else { paths.to_vec() };
```
When the LLM sends `paths: []`, this falls back to `git add -A`.

**Path 2 - Auto-commit in propose_bundle** (`bundle.rs:55-78`):
```rust
let auto_commit = tokio::process::Command::new("git")
    .args(["add", "-A"])
    .current_dir(worktree_path)
```
This is an unconditional safety net that catches uncommitted changes before creating the bundle. Even if the LLM committed correctly with targeted paths, this re-stages everything dirty in the worktree.

**The compound failure:** Loopr orchestration artifacts (`.taskstore/`, `.worktrees/`, `loopr.yml`) exist in the worktree because the daemon stores state inside the target repo's project root. These artifacts are not in the target's `.gitignore`. When `git add -A` runs, they get committed into the bundle alongside the intended work. The Reviewer rightfully rejects these polluted commits, creating infinite rejection loops. This is the confirmed root cause of the lua-todo E2E failure (independently diagnosed by two separate LLM analyses).

The broader issue: prompt-based scope enforcement will always be unreliable. LLMs don't perfectly follow instructions. Moving scope enforcement from prompt guidance to programmatic validation eliminates this entire class of failure.

### Goals

- Ensure only files matching a Work's `resource_tags` are staged and committed in bundles
- Report files modified but excluded from the bundle as `loose_files` for downstream observability
- Prevent Loopr orchestration artifacts from ever being staged, regardless of target repo `.gitignore`
- Preserve the existing auto-commit safety net (don't lose uncommitted work), but scope it
- Enable downstream agents (Reviewer, Coordinator) to learn from scope gaps and feed that knowledge back into future Work items

### Non-Goals

- Changing how `resource_tags` are defined or set on Work items (that's the Coordinator's job)
- Glob/regex expansion of resource_tags - keep them as literal paths for now, matching the plan YAML format (e.g., `todo.lua`, `cli.lua`, `test_todo.lua`)
- Modifying the integrator merge logic (it operates on whole branches, not individual files)
- Enforcing resource_tags on `write_file`/`edit_file` actions - the agent needs freedom to explore; enforcement happens at commit time, like a developer who reads broadly but stages deliberately

## Proposed Solution

### Overview

Four layers of defense, ordered from immediate to structural:

| Layer | What | Where | Effect |
|-------|------|-------|--------|
| 0 | `.git/info/exclude` for Loopr internals | Daemon init (root repo only) | Loopr artifacts invisible to git (inherited by worktrees) |
| 1 | Scoped `git add` in propose_bundle | `executor/action/bundle.rs` | Only resource_tags files staged at bundle time |
| 2 | Scoped fallback in explicit commit | `executor/action/file.rs` | No silent `-A` fallback when agent omits paths |
| 3 | Server-side scope validation | `daemon/handlers/bundle.rs` | Reject bundles with out-of-scope touched_paths |

Each layer catches what the previous one might miss. This mirrors how a veteran developer works: never stage something you didn't consciously decide to change.

### Architecture

```
                       .git/info/exclude (Layer 0)
                       Injected once at daemon init into ROOT repo's
                       .git/info/exclude. Worktrees inherit this
                       automatically (they share the common git dir).
                       Hides .taskstore/, .worktrees/, loopr.yml
                       from ALL git operations.
                                    |
                                    v
Implementer writes files (write_file, edit_file)
    |
    v
commit action (Layer 2)
    - paths provided by LLM?    -> git add <paths>
    - paths empty?              -> git status --porcelain
                                   partition_by_scope(dirty, resource_tags)
                                   git add <in_scope dirty files only>
    - resource_tags also empty? -> git add -A + warn log  (backward compat)
    |
    v
propose_bundle (Layer 1)
    - git status --porcelain  -> list all dirty files
    - partition_by_scope(dirty, resource_tags) -> (in_scope, out_of_scope)
    - git add <in_scope files only>
    - git commit (if staged changes exist)
    - out_of_scope files -> loose_files param to bundle.create
    |
    v
daemon bundle.create (Layer 3)
    - fetch Work.resource_tags from stores
    - normalize paths (strip ./ prefix) before comparison
    - validate touched_paths subset-of resource_tags
    - out-of-scope? -> warn log (initially), hard reject (after validation)
    - persist loose_files on Bundle record
    |
    v
Downstream visibility:
    Reviewer sees loose_files in review context
    Coordinator sees loose_files in triage
      -> can create Learning, widen resource_tags, or create new Work
```

### Data Model

#### Bundle struct addition

```rust
// In src/domain/bundle.rs
pub struct Bundle {
    // ... existing fields ...

    /// Files modified in the worktree but excluded from the bundle because
    /// they fall outside the Work's resource_tags scope. Observable signal
    /// for downstream agents to detect scope gaps.
    #[serde(default)]
    pub loose_files: Vec<String>,
}
```

#### Loopr exclude list

Constant set of patterns injected into `.git/info/exclude`:

```
# Loopr orchestration artifacts - auto-injected
.taskstore/
.worktrees/
loopr.yml
```

### API Design

#### IPC: bundle.create params

Add `loose_files` to the `bundle.create` RPC params:

```json
{
    "method": "bundle.create",
    "params": {
        "work_id": "wk-abc123",
        "branch_name": "agent/wk-abc123",
        "claims": ["Implemented todo.lua"],
        "touched_paths": ["todo.lua"],
        "loose_files": ["helpers.lua"],
        "loc_changed": 42
    }
}
```

Note: With Layer 0 in place, Loopr artifacts will never appear in `git status` output, so they won't reach `loose_files`. The daemon still filters known artifact patterns as belt-and-suspenders.

#### Internal: scope_check utility

New file: `src/agents/executor/action/scope.rs`

```rust
/// Loopr orchestration artifacts that should never be staged.
/// These are also in .git/info/exclude (Layer 0), but we filter
/// them here as defense in depth.
const LOOPR_ARTIFACTS: &[&str] = &[".taskstore/", ".worktrees/", "loopr.yml"];

/// Partition dirty files into in-scope and out-of-scope relative to resource_tags.
///
/// Returns (in_scope, out_of_scope). A file is in-scope if:
///   - It is NOT a Loopr artifact (always filtered, regardless of resource_tags)
///   - AND it matches at least one resource_tag as an exact path
///   - OR resource_tags is empty (backward compat: all non-artifact files are in-scope)
///
/// Both sides are normalized: leading "./" is stripped, paths are compared
/// case-sensitively (Unix convention).
pub fn partition_by_scope(
    dirty_files: &[String],
    resource_tags: &[String],
) -> (Vec<String>, Vec<String>) {
    let mut in_scope = Vec::new();
    let mut out_of_scope = Vec::new();
    for file in dirty_files {
        let normalized = file.strip_prefix("./").unwrap_or(file);
        let is_artifact = LOOPR_ARTIFACTS.iter().any(|a| normalized.starts_with(a));
        if is_artifact {
            out_of_scope.push(normalized.to_string());
            continue;
        }
        // Empty resource_tags = legacy mode: all non-artifact files are in-scope
        let matches_tag = resource_tags.is_empty() || resource_tags.iter().any(|tag| {
            let tag_norm = tag.strip_prefix("./").unwrap_or(tag);
            normalized == tag_norm
        });
        if matches_tag {
            in_scope.push(normalized.to_string());
        } else {
            out_of_scope.push(normalized.to_string());
        }
    }
    (in_scope, out_of_scope)
}

/// Parse `git status --porcelain` output into a list of file paths.
/// Handles status prefixes (M, A, D, ??, R, C) and quoted paths.
pub fn parse_porcelain_status(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            // Format: "XY filename" or "XY orig -> renamed"
            let rest = line.get(3..)?;
            // For renames/copies, take the destination (after " -> ")
            let path = rest.split(" -> ").last().unwrap_or(rest);
            Some(path.trim_matches('"').to_string())
        })
        .collect()
}
```

This is a pure, testable module used by both Layer 1 (propose_bundle) and Layer 2 (commit fallback).

### Implementation Plan

#### Layer 0: .git/info/exclude injection

**Where:** Daemon initialization (e.g., `src/daemon/context.rs` after `Store::open`, or equivalent startup path)

Inject Loopr patterns into the **root repository's** `.git/info/exclude` file once at daemon startup.

**Critical git mechanic:** Git worktrees share the *common git directory* with the root repo. The `info/exclude` file at `<repo>/.git/info/exclude` applies to all worktrees automatically. Per-worktree git directories (`.git/worktrees/<name>/`) do **not** have their own `info/exclude` - git ignores exclude files placed there. This means we only need to inject once into the root repo, not per-worktree.

```rust
const LOOPR_EXCLUDE_MARKER: &str = "# loopr-managed";
const LOOPR_EXCLUDES: &[&str] = &[".taskstore/", ".worktrees/", "loopr.yml"];

/// Ensure Loopr orchestration artifacts are in .git/info/exclude for the
/// given repository root. Idempotent: checks for marker before appending.
///
/// Because worktrees inherit the common git directory's exclude rules,
/// calling this once on the root repo covers all worktrees.
pub fn ensure_loopr_excludes(repo_path: &Path) -> Result<(), std::io::Error> {
    let exclude_path = repo_path.join(".git").join("info").join("exclude");

    // Read existing content (file may not exist yet)
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing.contains(LOOPR_EXCLUDE_MARKER) {
        return Ok(()); // Already injected
    }

    // Ensure .git/info/ directory exists
    if let Some(parent) = exclude_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Append our patterns
    let mut content = existing;
    if !content.ends_with('\n') && !content.is_empty() {
        content.push('\n');
    }
    content.push_str(&format!("{}\n", LOOPR_EXCLUDE_MARKER));
    for pattern in LOOPR_EXCLUDES {
        content.push_str(&format!("{}\n", pattern));
    }
    std::fs::write(&exclude_path, content)?;
    Ok(())
}
```

Call `ensure_loopr_excludes(&repo_path)` during daemon initialization, using the same `repo_path` that `WorktreeManager` is constructed with. This is the simplest possible change - one function, one call site.

**Note:** `WorktreeManager::create_branch()` does NOT need modification. The exclude rules are already in effect for any worktree created after daemon init. If a worktree was created before the exclude was injected (e.g., daemon restart), it still inherits the updated exclude because worktrees share the common git directory.

**Files changed:**
- `src/worktree/manager.rs` (or a shared `git` utility module) - add `ensure_loopr_excludes()`
- `src/daemon/mod.rs` (or startup path) - call `ensure_loopr_excludes(&repo_path)` at init

#### Layer 1: Scoped git add in propose_bundle

**Where:** `handle_propose_bundle()` in `src/agents/executor/action/bundle.rs:55-78`

Replace the unconditional `git add -A` block with:

```rust
// 1. Fetch the Work's resource_tags
let resource_tags = fetch_resource_tags(bridge, wi_id);

// 2. Get dirty files
let status_output = tokio::process::Command::new("git")
    .args(["status", "--porcelain"])
    .current_dir(worktree_path)
    .output().await?;
let dirty_files = scope::parse_porcelain_status(
    &String::from_utf8_lossy(&status_output.stdout)
);

// 3. Partition by scope.
// partition_by_scope always filters Loopr artifacts, even when resource_tags
// is empty (in that case, all non-artifact dirty files are treated as in-scope).
let (in_scope, out_of_scope) = scope::partition_by_scope(&dirty_files, &resource_tags);
if resource_tags.is_empty() {
    agent_log.warn("Work has no resource_tags, staging all non-artifact dirty files");
}

// 4. Stage only in-scope files
if !in_scope.is_empty() {
    let mut add_cmd = tokio::process::Command::new("git");
    add_cmd.arg("add").args(&in_scope).current_dir(worktree_path);
    add_cmd.output().await?;

    // 5. Commit staged changes
    tokio::process::Command::new("git")
        .args(["commit", "-m", &format!("impl: {}", description)])
        .current_dir(worktree_path)
        .output().await?;
    agent_log.info("Auto-committed in-scope changes before propose_bundle");
}

// 6. Pass loose_files to bundle.create
if !out_of_scope.is_empty() {
    agent_log.info(&format!("Loose files (not in scope): {:?}", out_of_scope));
    params["loose_files"] = serde_json::json!(out_of_scope);
}
```

**Edge case - empty resource_tags:** If `resource_tags` is empty (legacy or ad-hoc Work without a plan YAML), `partition_by_scope` still filters out Loopr artifacts but treats all other dirty files as in-scope. This preserves backward compatibility while ensuring artifacts are never staged, even if Layer 0's `.git/info/exclude` injection fails.

**Files changed:**
- `src/agents/executor/action/bundle.rs` - rewrite auto-commit block (lines 55-78)
- `src/agents/executor/action/scope.rs` (new) - `partition_by_scope()`, `parse_porcelain_status()`
- `src/agents/executor/action/mod.rs` - add `mod scope;`

#### Layer 2: Scoped fallback in explicit commit

**Where:** `handle_commit()` in `src/agents/executor/action/file.rs:189-191`

Change the signature to accept `worktree_path` and `resource_tags`, and use the same `git status` + `partition_by_scope` logic as Layer 1 when the LLM provides no explicit paths:

```rust
pub(super) async fn handle_commit(
    worktree_path: &Path,
    message: &str,
    paths: &[String],
    resource_tags: &[String],  // NEW: from Work, passed through execute_action
) -> Result<ActionResult> {
    let add_args = if !paths.is_empty() {
        // LLM explicitly specified files - trust them
        paths.to_vec()
    } else if !resource_tags.is_empty() {
        // No paths from LLM - scope to resource_tags via git status.
        // We CANNOT blindly pass resource_tags to git add because files
        // that haven't been modified or don't exist will cause git add
        // to fail with "fatal: pathspec did not match any files".
        let status_output = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree_path)
            .output()
            .await?;
        let dirty_files = scope::parse_porcelain_status(
            &String::from_utf8_lossy(&status_output.stdout)
        );
        let (in_scope, _) = scope::partition_by_scope(&dirty_files, resource_tags);
        if in_scope.is_empty() {
            return Err(eyre!("no in-scope dirty files to commit"));
        }
        in_scope
    } else {
        log::warn!("commit with no paths and no resource_tags, falling back to -A");
        vec!["-A".to_string()]
    };
    // ... rest unchanged (git add <add_args>, then git commit) ...
}
```

The caller in `execute_action()` (`mod.rs:54`) already has `ctx` and `work_id`. Fetch resource_tags from the bridge and pass them through:

```rust
AgentAction::Commit { message, paths } => {
    let resource_tags = work_id
        .and_then(|wid| fetch_resource_tags(&ctx.bridge, wid))
        .unwrap_or_default();
    file::handle_commit(worktree_path, message, paths, &resource_tags).await
}
```

**Why not blindly pass resource_tags to git add:** If `resource_tags` contains `["todo.lua", "cli.lua"]` but only `todo.lua` was actually modified, running `git add todo.lua cli.lua` will fail with `fatal: pathspec 'cli.lua' did not match any files`. The `git status` + `partition_by_scope` approach only stages files that are both dirty AND in-scope - exactly the right set.

**Files changed:**
- `src/agents/executor/action/file.rs` - change `handle_commit` signature, use scoped status logic
- `src/agents/executor/action/mod.rs` - thread resource_tags at call site

#### Layer 3: Server-side scope validation in bundle.create

**Where:** `handle_bundle_create()` in `src/daemon/handlers/bundle.rs`

After BundleSizePolicy enforcement (line 179), before persisting:

```rust
/// Normalize a path for scope comparison: strip leading "./" prefix.
fn normalize_path(p: &str) -> &str {
    p.strip_prefix("./").unwrap_or(p)
}

// Scope validation: touched_paths must be subset of Work's resource_tags.
// Uses the same normalization as partition_by_scope (strip leading "./")
// to avoid false rejections from path format differences.
let work = stores.read_works()?.get(&work_id).cloned();
if let Some(ref w) = work {
    if !w.resource_tags.is_empty() && !bundle.touched_paths.is_empty() {
        let violations: Vec<&str> = bundle.touched_paths.iter()
            .filter(|p| {
                let norm_p = normalize_path(p);
                !w.resource_tags.iter().any(|tag| normalize_path(tag) == norm_p)
            })
            .map(|p| p.as_str())
            .collect();
        if !violations.is_empty() {
            // Phase 1: warn only (log but don't reject).
            // Phase 2: flip to hard rejection after E2E validation.
            log::warn!(
                "Bundle {} touches files outside Work {}'s resource_tags: {:?}. Allowed: {:?}",
                bundle.id, work_id, violations, w.resource_tags
            );
            // TODO(phase2): Uncomment to enforce hard rejection:
            // return Ok(DaemonResponse::err(
            //     req.id,
            //     RpcError::precondition_failed(&format!(
            //         "Bundle touches files outside Work's resource_tags: {:?}. \
            //          Allowed: {:?}",
            //         violations, w.resource_tags
            //     )),
            // ));
        }
    }
}

// Parse and persist loose_files
if let Some(files) = req.params.get("loose_files").and_then(|v| v.as_array()) {
    bundle.loose_files = files.iter()
        .filter_map(|v| v.as_str().map(String::from))
        // Filter out known Loopr artifacts (belt-and-suspenders with Layer 0)
        .filter(|f| !LOOPR_ARTIFACTS.iter().any(|a| f.starts_with(a)))
        .collect();
}
```

**Important:** The daemon validates `touched_paths` as received in the RPC params - it does not re-compute them from the filesystem. The agent is responsible for accurately reporting `touched_paths` (already the case today). Layer 3 validates that what the agent reports is within scope.

**Rollout alignment:** Layer 3 starts as a warning log (phase 1). After E2E validation confirms Layers 0-2 produce correct `touched_paths` without false positives, flip to hard rejection (phase 2) by uncommenting the `DaemonResponse::err` block.

**Files changed:**
- `src/daemon/handlers/bundle.rs` - add scope validation after size policy block
- `src/domain/bundle.rs` - add `loose_files: Vec<String>` field with `#[serde(default)]`

#### Prompt updates

Update `prompts/reviewer.pmt` and `prompts/coordinator.pmt` to reference loose_files:

- **Reviewer:** "If the bundle has `loose_files`, the implementer also modified these files but they were excluded because they fall outside the Work's `resource_tags`. Consider whether the scope was too narrow or if these changes are unrelated."
- **Coordinator:** "When triaging a bundle with non-empty `loose_files`, consider: (a) creating a Learning noting the scope gap, (b) widening `resource_tags` on the Work if the files are genuinely needed, or (c) creating a new Work item for the additional files."

**Files changed:**
- `prompts/reviewer.pmt`
- `prompts/coordinator.pmt`

## Alternatives Considered

### Alternative 1: .gitignore in target repo

- **Description:** Add Loopr artifacts to the target repo's tracked `.gitignore` file
- **Pros:** Simple, works immediately
- **Cons:** Modifies the user's tracked files; every E2E target needs its own fix; doesn't solve the general problem of out-of-scope files
- **Why not chosen:** `.git/info/exclude` is invisible to the user's repo and handles all targets automatically. It also means Loopr doesn't need to "own" a line in the user's `.gitignore`.

### Alternative 2: Prompt-only enforcement (status quo)

- **Description:** Keep telling the LLM "don't modify files outside resource_tags"
- **Pros:** Zero code changes
- **Cons:** LLMs don't perfectly follow instructions. This is the current state and it doesn't work - the lua-todo E2E failure proves it.
- **Why not chosen:** Prompt guidance is necessary but not sufficient. Programmatic enforcement is required.

### Alternative 3: Sandbox write_file/edit_file to resource_tags

- **Description:** Reject file writes outside resource_tags at action execution time
- **Pros:** Catches scope violations earliest
- **Cons:** Too restrictive. The agent may need to read files outside scope to understand the codebase, or create temporary helper files while iterating. Enforcement at commit time is the right boundary - like a developer who explores freely but stages deliberately.
- **Why not chosen:** Commit-time enforcement matches the human workflow and preserves agent flexibility.

### Alternative 4: Post-merge validation only

- **Description:** Let bundles through but reject at integration if out-of-scope
- **Pros:** Simple server-side check
- **Cons:** Wastes the entire review cycle before catching the error. A bundle that touches 15 files when it should touch 1 will still consume Reviewer tokens and time.
- **Why not chosen:** Layer 3 validates at bundle.create, which is strictly earlier and cheaper.

### Alternative 5: Track modified files from write_file/edit_file actions

- **Description:** Instead of `git status`, use the executor's knowledge of which files the agent wrote/edited to determine what to stage
- **Pros:** Doesn't rely on git; knows exactly what the agent intended
- **Cons:** The agent might also create files via `run_tool` (e.g., a build tool generating output), which wouldn't be tracked. Git status is the ground truth for "what changed."
- **Why not chosen:** Git status is more comprehensive. However, tracking agent-written files could be a future enhancement for richer loose_files reporting (distinguishing "agent wrote this" from "something else changed this").

## Technical Considerations

### Dependencies

- No new external dependencies
- Uses existing `AgentContext.bridge` for RPC calls to fetch Work
- `partition_by_scope` and `parse_porcelain_status` are pure functions with no dependencies

### Performance

- Layer 0: One-time file write per worktree creation (negligible)
- Layer 1: One `git status --porcelain` + one `git add <files>` instead of `git add -A` (same or faster - targeted add can be faster than -A on large repos)
- Layer 2: No additional git commands; just changes which paths are passed to `git add`
- Layer 3: One HashMap lookup for Work + Vec iteration (negligible)
- `resource_tags` fetch via bridge: synchronous RPC to daemon stores (already in-memory HashMap, no disk I/O)

### Security

- `.git/info/exclude` prevents accidental exposure of Loopr's internal state (`.taskstore/` contains work assignments, learnings, agent logs - sensitive orchestration data)
- Server-side validation (Layer 3) prevents a misbehaving agent from polluting main with out-of-scope changes
- The defense-in-depth approach means no single layer failure compromises the system

### Testing Strategy

- **Unit tests for `partition_by_scope`**: exact match, no match, empty tags, empty files, mixed, leading `./` normalization, Loopr artifact filtering
- **Unit tests for `parse_porcelain_status`**: modified files, added files, deleted files, renamed files (` -> ` syntax), untracked (`??`) files, quoted paths with spaces
- **Unit tests for `ensure_loopr_excludes`**: file doesn't exist, file exists without marker, file exists with marker (idempotent re-run), `.git/info/` directory doesn't exist, verify worktrees inherit root exclude
- **Integration test for Layer 3**: send a `bundle.create` RPC with `touched_paths` containing an out-of-scope file, verify warning log (phase 1) or RPC error (phase 2); test with `./`-prefixed paths to verify normalization
- **E2E validation**: re-run lua-todo E2E after Layer 0+1, verify it passes without `.taskstore` in commits

### Rollout Plan

1. **Layer 0 first** - unblocks all E2E targets immediately with zero behavioral change to agent logic. This is a 1-file change plus a call in daemon init.
2. **Layer 1 next** - scoped staging in propose_bundle; backward compatible (falls back to -A if no resource_tags). This is the structural fix.
3. **Layer 2** - scoped fallback in commit; requires signature change but low risk (only one call site in `mod.rs:54`).
4. **Layer 3 last** - server-side gate; start as a warning log, then enforce after E2E validation confirms layers 0-2 work.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| resource_tags is empty on legacy/ad-hoc Work | Medium | Medium | Fall back to `git add -A` with warning log; don't break existing flows |
| Agent writes necessary files outside resource_tags (e.g., creates helpers.lua for todo.lua) | Medium | Low | loose_files makes this visible; Coordinator can widen tags or create new Work |
| `.git/info/exclude` injection fails (permissions, missing info/ dir) | Low | Medium | Create `.git/info/` dir; log warning on failure; Layer 1 still catches artifacts |
| Path normalization mismatch (`./ prefix`, trailing slash) | Medium | Medium | Shared `normalize_path()` applied in `partition_by_scope` AND Layer 3 daemon validation |
| Layer 2: resource_tags contains files not yet created/modified | Medium | High | NEVER pass resource_tags directly to `git add` - always filter through `git status` + `partition_by_scope` first (see Layer 2 implementation) |
| Commit signature change breaks callers | Low | Low | Only one call site (`mod.rs:54`); verified by grep |
| Agent reports inaccurate touched_paths in RPC | Low | Medium | Layer 3 validates what's reported; future enhancement could verify against actual diff |

## Open Questions

- [ ] Should `resource_tags` support glob patterns (e.g., `src/**/*.rs`) in the future? Current plan YAMLs use literal paths only (`todo.lua`, `cli.lua`). Glob support adds power but also complexity. Recommendation: start with literal match, add glob as a follow-up if needed.
- [ ] Should Layer 3 reject outright from day one or start as a warning? Recommendation: warning first, then enforce after validating with E2E suite.
- [ ] Should non-empty `loose_files` trigger automatic Learning creation, or leave that judgment to the Coordinator? Recommendation: leave it to the Coordinator - it has the context to decide whether the scope gap is systemic or one-off.

## Review Log

### Architect Review (2026-04-02) - Gemini

Two critical bugs found and fixed:

1. **Layer 0: Worktree-specific `info/exclude` is ignored by Git.** Empirically verified that git only respects the common git directory's exclude file (`<repo>/.git/info/exclude`), not per-worktree exclude files at `.git/worktrees/<name>/info/exclude`. **Fix:** Inject once into root repo's `.git/info/exclude` during daemon init. Worktrees inherit automatically. This actually simplifies the implementation - no per-worktree logic needed.

2. **Layer 2: Blind `git add <resource_tags>` crashes on unmodified/missing files.** If `resource_tags` contains `["todo.lua", "cli.lua"]` but only `todo.lua` was modified, `git add cli.lua` fails with `fatal: pathspec 'cli.lua' did not match any files`. **Fix:** Layer 2 must use the same `git status --porcelain` + `partition_by_scope` pattern as Layer 1. Never pass resource_tags directly to `git add`.

Additional refinements applied:
- Layer 3 path normalization now uses shared `normalize_path()` to strip `./` prefixes before comparison
- Layer 3 code aligned with rollout plan: warn-first (phase 1), hard reject after validation (phase 2)
- Confirmed: `parse_porcelain_status` rename handling (`" -> "` split) is correct

### Architect Review Follow-up (2026-04-02) - Gemini

One edge case in Layer 1's empty `resource_tags` fallback: if Layer 0's `.git/info/exclude` injection fails AND `resource_tags` is empty, the fallback `(dirty_files.clone(), vec![])` would stage Loopr artifacts. **Fix:** `partition_by_scope` now handles empty `resource_tags` natively - it always filters artifacts first, then treats all remaining dirty files as in-scope when tags are empty. Callers no longer branch around `partition_by_scope`; they always call it.

## References

- `src/agents/executor/action/bundle.rs:55-78` - current auto-commit (the bug)
- `src/agents/executor/action/file.rs:189-191` - current commit fallback (secondary bug)
- `src/agents/executor/action/mod.rs:54` - single call site for handle_commit
- `src/domain/bundle.rs:64-93` - Bundle struct
- `src/domain/work.rs:44-60` - Work struct with resource_tags
- `src/daemon/handlers/bundle.rs:30-203` - bundle.create handler
- `src/worktree/manager.rs:59-109` - WorktreeManager::create_branch
- `src/daemon/work_queue.rs:68-88` - resource_tags scheduling usage
- `src/agents/context.rs:415-420` - resource_tags in reviewer context
- `prompts/implementer.pmt:58` - prompt-only scope enforcement
- `bin/e2e-targets/lua-todo.yml:48,86,121` - resource_tags in plan YAML
- `docs/design/2026-02-25-orchestration-spine.md` - daemon/IPC architecture
