# Design Document: Scoped Staging for `commit_changes`

**Author:** Claude (with Scott)
**Date:** 2026-04-26
**Status:** Implemented
**Review Passes Completed:** 5/5
**Architect round:** 1 (2026-04-26; revisions applied below in Architect Round 1 Findings)
**Crates touched:** agents, decomposer, domain, context

## Summary

Restore v4's scoped-staging behavior to v5's `dispatch.rs`. Replace unconditional `git add -A` in both `commit_changes` and `propose_bundle` with a `git status --porcelain` + filter pipeline, and have the decomposer emit a per-Work `files` list that dispatch uses as the staging allow-list. Defense-in-depth against unintended commits (the `.venv/` regression observed on 2026-04-26) and a structural enforcement of Work scope.

## Problem Statement

### Background

The implementer agent's `commit_changes` action stages worktree changes via git. The current v5 implementation (`crates/agents/src/dispatch.rs:118`) uses unconditional `git add -A`, with a duplicate identical call inside `propose_bundle` (line 157) when the staging area is dirty at proposal time.

This is a regression from v4. The lineage:

- **v3** (`~/repos/scottidler/loopr/src/agents/executor.rs:509-529`): `git add -A` as the unconditional fallback when no explicit paths were provided.
- **v4** (`~/repos/scottidler/loopr-v4/src/agents/executor/action/file.rs:188-239` + `scope.rs`): three-tier staging:
  1. If the LLM provided explicit `paths`, trust them.
  2. Else if the Work has `files` resource tags, run `git status --porcelain`, parse it, then `partition_by_scope(dirty_files, work.files)` to drop loopr internal artifacts unconditionally and out-of-scope files when scope is non-empty.
  3. Else fall back to `-A` with a `tracing::warn!` so the caller sees the unsafe path.
- **v5** (current): regressed all the way back to unconditional `-A`. The dispatch function does not even take the Work as input, so it cannot know what files are in scope.

### Problem

On 2026-04-26 the `python-api` e2e target produced a 259-second run with one of three Works (`wk-ryqmy`, FastAPI `main.py`) blocked by a reviewer rejection. The reviewer's rationale, quoted verbatim from the events log:

> The diff contains only .venv virtual environment files and no application code (main.py, tests, requirements.txt, etc.) - the actual Work deliverable is entirely absent

The actual diff contained 530,824 lines of `.venv/` files plus the legitimate `main.py` and `test_api.py` - the scaffold's `.gitignore` did not include `.venv/`, the implementer ran `uv sync`, and `git add -A` then staged the entire virtual environment alongside the real code. The reviewer correctly rejected the bundle because the `.venv/` noise dominated the diff.

A second Work (`wk-emdu3`) was blocked by an unrelated lifeguard escalation (a Rust `tail` binary on `PATH` panicking on `tail -5`, since fixed externally), but it would have hit the same staging problem on its happy path.

### Goals

- Replace `git add -A` in `commit_changes` and `propose_bundle` with a deterministic, filterable staging pipeline.
- Restore the scope contract: a Work decomposed to touch `["main.py", "test_api.py"]` cannot accidentally commit changes to `database.py` or to a build artifact directory.
- Provide defense-in-depth that does not depend on the scaffold author predicting every artifact path the agent might generate.
- Keep the dispatch layer testable in isolation (no taskstore, no LLM).
- Keep the change a clean rewrite of the relevant functions; do not add a coexistence flag.

### Non-Goals

- Changing the reviewer's diff-evaluation logic. The reviewer continues to receive a diff; this work changes only what gets staged into that diff.
- Removing `.gitignore` from the scaffold. `.gitignore` and scoped staging are complementary layers; both remain.
- Per-file LLM staging hints (the v4 "explicit paths from the LLM" branch). The decomposer's `files` field is sufficient; per-action paths add complexity for no measured win.
- Renaming or moving `dispatch_action`. The function signature change is additive only.
- Extending scope enforcement to `RunTool` (file writes outside scope). Out of scope here; if needed, a separate design.

## Proposed Solution

### Overview

Two parts that ship together but are conceptually separable:

1. **Artifact-aware staging.** Replace `git add -A` with `git status --porcelain` parse + filter. Drop loopr internal artifacts unconditionally. This is the v4 `partition_by_scope` lower half, ported to v5's artifact set (`.loopr/`).
2. **Scope enforcement.** Have the decomposer emit a `files` array per child Work, persist it to `Work.files` (the field already exists on `domain::Work`), and thread `Work.files` into `dispatch_action` so `commit_changes` and `propose_bundle` can intersect the porcelain output with the allow-list.

When `Work.files` is empty (legacy / decomposer ran without the new prompt), fall back to artifact-only filtering. This keeps the dispatch crate's behavior coherent for callers that don't yet emit scope.

### Architecture

Two crates change code (`agents`, `decomposer`); one crate gets a prompt template edit (`context`); one crate is affected only through dormant fields (`domain` - both `Work.files` and `Bundle.paths` exist today as `#[serde(default)] Vec<String>` and gain producers without any struct change):

```
crates/context/prompts/decompose/work/system.pmt   # add `files` to the Work schema
crates/decomposer/src/tool.rs                      # DecomposeChild gains `files: Vec<String>`
crates/decomposer/src/decompose.rs                 # populate work.files from child.files
crates/agents/src/dispatch.rs                      # commit_changes + propose_bundle take &Work
crates/agents/src/implementer.rs                   # caller threads &work into dispatch_action
crates/agents/src/scope.rs                         # NEW: parse_porcelain + partition_by_scope
```

The scope module is a near-verbatim port of v4's `scope.rs` with one substitution: the `LOOPR_ARTIFACTS` constant changes from `[".taskstore/", ".worktrees/", "loopr.yml"]` (v4) to `[".loopr/"]` (v5; the entire orchestrator state lives under one directory in v5).

The artifact filter is **mostly defensive** in v5: `commit_changes` runs `git status --porcelain` from inside the per-Work git worktree (`.loopr/worktrees/wk-X-N/`), and a worktree's status output never contains paths to its parent `.loopr/` directory. The filter is therefore a no-op on the expected path and costs nothing. It exists for two reasons: (1) parity with v4's defense-in-depth posture so a future misconfiguration cannot regress silently, and (2) it lets the same `partition_by_scope` function serve callers that may someday operate on a non-worktree git repo (no current callers, but the function should not have to be rewritten if one appears).

### Index ownership: `git commit --only`

`commit_changes` and `propose_bundle` do NOT own the worktree's git index. The implementer agent can run arbitrary tools, including `git add` invocations from `bash`, that may stage paths outside the Work's scope. A naive `git add -- <in_scope> && git commit` would silently land any out-of-scope changes already in the index because `git commit` commits the entire index by default.

**Decision:** use `git commit --only --message <msg> --no-gpg-sign -- <in_scope_paths>`. With `--only`, git creates a commit containing exactly the working-tree changes for the listed paths and ignores everything else in the index. Per `git-commit(1)`:

> When pathspecs and --only are given, the new commit will only contain changes made to those paths. The contents of those files in the index will be reset to that of the new commit.

Consequences of this decision:

- **No separate `git add` step.** `--only` snapshots the working-tree contents of the listed paths directly into the commit and updates the index entries for those paths only. This eliminates the index-leak class of bugs where prior `git add` invocations from `bash` actions would otherwise be folded in.
- **Stale staged out-of-scope entries persist.** If the agent staged `database.py` outside our flow and we commit `--only -- main.py`, the index entry for `database.py` survives the commit. This is acceptable: subsequent `commit_changes` calls re-run the porcelain partition and again exclude `database.py`. The stale index state never reaches a commit. `propose_bundle`'s `bundle.paths` is computed from `git diff --name-only <base>..HEAD`, which sees only what was actually committed, so the stale state does not contaminate the bundle either.
- **Rename collapses cleanly.** The Pass-4 concern about porcelain `R old -> new` lines (only emitted for already-staged renames) loses its teeth: `git commit --only -- old new` either covers both sides if both are in scope, or covers neither if both are out of scope. The bundled rename either lands fully or is dropped fully. There is no half-rename failure mode. (`parse_porcelain_status` still extracts both paths so `partition_by_scope` sees both, but the resolution happens in the commit, not in a separate `git add` step.)
- **`--untracked-files=all` is mandatory.** `git status --porcelain` rolls newly-created directories into a single `?? new_dir/` entry by default. `partition_by_scope` does exact-path matching, so `["new_dir/main.py"]` would fail to match `new_dir/`. The helper MUST run `git status --porcelain --untracked-files=all` so each new file is enumerated individually.

### Data Model

`domain::Work.files` already exists as `Vec<String>` with `#[serde(default)]`. No change.

`domain::Bundle.paths` already exists as `Vec<String>`, default-constructed empty by `Bundle::new`. **No production code currently populates it**, but the reviewer and integrator already read it as if it were populated:

- `agents/src/reviewer.rs:143` passes it to `git_show(target, sha, &bundle.paths)`. `git_show` (line 426) appends `-- <paths>` to the `git show` command when `paths` is non-empty, filtering the diff to only those paths. With the current empty value, the reviewer receives the full unfiltered diff. **This is precisely how `.venv/` reached the reviewer in the 2026-04-26 run** even though scope-tag information would have allowed filtering.
- `integrator/src/classify.rs:35-45` uses `bundle.paths` to detect path collisions between concurrently-merging bundles. With empty paths the classifier silently treats every bundle as touching nothing.

After this change, `propose_bundle` populates `bundle.paths` with the actual in-scope staged paths. This has two downstream effects without any further code change: (1) the reviewer's diff is automatically filtered to in-scope files, and (2) the integrator's collision detector becomes meaningful. v4 populated `paths` for exactly these reasons; v5 left it empty as part of the staging regression.

### `ActionResult` schema: agent-visible scope feedback

The Architect round 1 review surfaced a critical gap: `tracing::warn!` emissions are invisible to the LLM. The current `ActionResult::NothingToCommit` variant maps to a flat string in the agent's iteration history (`implementer.rs` ~lines 280-340), so an agent whose entire dirty set was out-of-scope receives `"commit_changes: nothing to commit"` and has no signal that its files were dropped or why. It will retry the same out-of-scope edit, hit the lifeguard's same-action escalation, and block the Work - exactly the regression class this design was meant to prevent.

The fix is to extend the result variants with the dropped path set so the iteration-history rendering surfaces them to the LLM:

```rust
// crates/agents/src/dispatch.rs

pub enum ActionResult {
    ToolOutput(String),
    /// `CommitChanges` succeeded. `dropped` lists out-of-scope paths
    /// the partition filter excluded from this commit; non-empty
    /// `dropped` is a soft warning surfaced to the agent.
    Committed { sha: String, dropped: Vec<String> },
    /// `CommitChanges` produced no commit. `dropped` lists paths the
    /// partition filter excluded; if non-empty the agent should
    /// either edit in-scope or emit `need_help`.
    NothingToCommit { dropped: Vec<String> },
    /// `ProposeBundle` succeeded. `dropped` lists out-of-scope paths
    /// left uncommitted in the worktree.
    BundleCreated { bundle: Bundle, dropped: Vec<String> },
    Done(Bundle),
    NeedHelp(String),
    Error(String),
}
```

Iteration-history rendering in `implementer.rs` is updated so that a non-empty `dropped` set produces a structured note the LLM can read:

```
commit_changes: committed <sha>
note: 3 out-of-scope path(s) were dropped from this commit because they
are not in the Work's `files` scope: ["unrelated.py", "shared/util.py", "data/seed.csv"]
The Work's scope is: ["main.py", "test_api.py"]
If you need to edit those files, emit `need_help` with the reason.
```

Empty `dropped` produces the existing terse summary. The verbose form fires only when there's information the agent needs.

This is the structural answer to the "agent must be able to react to scope filtering" concern that `tracing::warn!` cannot satisfy.

### API Design

```rust
// crates/agents/src/scope.rs (new file)

const LOOPR_ARTIFACTS: &[&str] = &[".loopr/"];

/// Parse `git status --porcelain` output into a list of file paths.
/// Handles M, A, D, ??, R, C status prefixes and quoted paths.
pub fn parse_porcelain_status(output: &str) -> Vec<String> { /* same as v4 */ }

/// Returns (in_scope, out_of_scope). A file is in-scope if:
///   - It is NOT a loopr artifact (always filtered, regardless of scope_files)
///   - AND it matches at least one entry in scope_files as an exact path
///   - OR scope_files is empty (artifact-only filtering)
pub fn partition_by_scope(
    dirty_files: &[String],
    scope_files: &[String],
) -> (Vec<String>, Vec<String>) { /* same as v4 */ }
```

```rust
// crates/agents/src/dispatch.rs (signature change)

pub async fn dispatch_action<T: ToolExecutor>(
    action: AgentAction,
    work: &Work,                  // NEW
    worktree: &Worktree,
    tools: &T,
) -> Result<ActionResult, DispatchError>;

async fn commit_changes(
    path: &Path,
    scope_files: &[String],       // NEW: from work.files
    message: &str,
) -> Result<ActionResult, DispatchError>;

async fn propose_bundle(
    worktree: &Worktree,
    scope_files: &[String],       // NEW
    claims: Vec<String>,
) -> Result<ActionResult, DispatchError>;

// Helpers (private, in dispatch.rs)

/// Run `git status --porcelain --untracked-files=all` and parse via
/// `scope::parse_porcelain_status`. The `--untracked-files=all` flag
/// is mandatory: without it, untracked directories collapse to a
/// single `?? new_dir/` entry that scope-tag exact matching cannot
/// resolve to a file path.
async fn git_status_porcelain(path: &Path) -> Result<Vec<String>, DispatchError>;

/// Run `git diff --name-only <base_sha>..HEAD`. Used by
/// `propose_bundle` to populate `bundle.paths` with the canonical
/// set of paths the bundle's branch touched (not just the last
/// staging step).
async fn git_diff_name_only(path: &Path, base_sha: &str) -> Result<Vec<String>, DispatchError>;
```

```rust
// crates/decomposer/src/tool.rs (schema change)

pub(crate) struct DecomposeChild {
    pub title: String,
    #[serde(default)] pub content: String,
    #[serde(default)] pub dependencies: Vec<String>,
    #[serde(default)] pub acceptance_criteria: Vec<String>,
    #[serde(default)] pub files: Vec<String>,        // NEW
}
```

The `submit_decomposition` JSON schema gains a per-child `files` field, optional (the decomposer falls back to artifact-only filtering when omitted), described as "Files this Work is expected to create or modify, relative to the worktree root."

### Target shape: `commit_changes`

For implementer reference, the target body of `commit_changes`. Note the use of `git commit --only -- <paths>` (not separate `git add` + `git commit`) per the index-ownership decision above:

```rust
async fn commit_changes(
    path: &Path,
    scope_files: &[String],
    message: &str,
) -> Result<ActionResult, DispatchError> {
    let dirty = git_status_porcelain(path).await?;  // uses --untracked-files=all
    if dirty.is_empty() {
        return Ok(ActionResult::NothingToCommit { dropped: vec![] });
    }
    let (in_scope, out_of_scope) = scope::partition_by_scope(&dirty, scope_files);
    if !out_of_scope.is_empty() {
        warn!(
            out_of_scope = ?out_of_scope,
            "commit_changes: dropping out-of-scope dirty paths"
        );
    }
    if in_scope.is_empty() {
        return Ok(ActionResult::NothingToCommit { dropped: out_of_scope });
    }
    // Stage only the in-scope paths first so untracked new files
    // become known to git: `git commit --only` will not promote an
    // untracked file into the index by itself (verified empirically
    // 2026-04-27; an early Phase-3 build that omitted this step
    // failed the new-file test with "pathspec did not match any
    // file(s) known to git").
    let mut add_args = vec!["add", "--"];
    add_args.extend(in_scope.iter().map(String::as_str));
    run_git(path, &add_args).await?;
    // git commit --only -- <paths>: snapshots the working-tree contents
    // of <paths> into a commit, ignoring any other index entries.
    // Eliminates the index-leak class of bugs where prior `git add`
    // invocations from bash actions would otherwise be folded in. The
    // preceding `git add -- <in_scope>` does NOT defeat `--only`: any
    // out-of-scope index entries from `bash: git add ...` are still
    // ignored at commit time.
    let mut commit_args = vec![
        "commit", "--only",
        "--message", message,
        "--no-gpg-sign", "--",
    ];
    commit_args.extend(in_scope.iter().map(String::as_str));
    run_git(path, &commit_args).await?;
    let sha = rev_parse_head(path).await?;
    Ok(ActionResult::Committed { sha, dropped: out_of_scope })
}
```

`propose_bundle`'s staging block follows the same pattern. After staging, `bundle.paths` is populated from the branch-vs-base diff (not just the last staging step), so the reviewer and integrator see every path the bundle touched - including paths committed by earlier `commit_changes` actions on the same iteration. The result returns `BundleCreated { bundle, dropped }` so the agent sees any out-of-scope paths it left uncommitted in the worktree:

```rust
let mut total_dropped: Vec<String> = vec![];
if !is_working_tree_clean(worktree.path()).await? {
    let dirty = git_status_porcelain(worktree.path()).await?;
    let (in_scope, out_of_scope) = scope::partition_by_scope(&dirty, scope_files);
    if !out_of_scope.is_empty() {
        warn!(out_of_scope = ?out_of_scope, "propose_bundle: dropping out-of-scope");
        total_dropped = out_of_scope;
    }
    if !in_scope.is_empty() {
        let mut commit_args = vec![
            "commit", "--only",
            "--message", "propose_bundle: stage remaining changes",
            "--no-gpg-sign", "--",
        ];
        commit_args.extend(in_scope.iter().map(String::as_str));
        run_git(worktree.path(), &commit_args).await?;
    }
}
let head_commit = rev_parse_head(worktree.path()).await.ok();
let loc_changed = compute_loc_changed(worktree.path(), worktree.sha()).await.ok();
// Branch-vs-base diff: the canonical set of paths this bundle touches.
let branch_paths = git_diff_name_only(worktree.path(), worktree.sha()).await.unwrap_or_default();
let mut bundle = Bundle::new(worktree.work_id().clone(), worktree.branch().to_string(), claims);
bundle.head_commit = head_commit;
bundle.loc_changed = loc_changed;
bundle.paths = branch_paths;        // NEW: full bundle scope, not just last staging step
Ok(ActionResult::BundleCreated { bundle, dropped: total_dropped })
```

Empty-scope behavior: `partition_by_scope` returns `(in_scope = all_non_artifact, out_of_scope = all_artifact)` when `scope_files` is empty, so the helper is the single staging path - no separate code branch for the legacy-empty case.

### Rename and deletion handling

`git status --porcelain` rename lines have shape `R  old -> new` and are emitted only when the rename is already staged. `parse_porcelain_status` extracts both sides for `R` and `C` (copy) entries so `partition_by_scope` can evaluate both for scope membership. Add a unit test asserting a rename produces two paths.

Under the index-ownership decision (`git commit --only -- <paths>`), the rename concern from v4 collapses cleanly:

- If both `old` and `new` are in scope, `git commit --only -- old new` lands the full rename in one commit.
- If both are out of scope, neither is staged and the rename stays in the index but never reaches a commit.
- If exactly one is in scope (rare; would be a decomposer mistake), partition returns one path, `git commit --only -- <one>` produces a partial commit, and the agent sees the unmatched path in `dropped` so it can `need_help`.

Deletions (` D filename`) are returned as the single path. `git commit --only -- <path>` correctly snapshots a deletion (the working-tree absence is the change), so no special handling is needed.

### Implementation Plan

#### Phase 1: Port `scope.rs` to v5 (with rename fix)
**Model:** sonnet
- Create `crates/agents/src/scope.rs` with `parse_porcelain_status` and `partition_by_scope`. Lift the v4 implementation; substitute `LOOPR_ARTIFACTS = &[".loopr/"]`.
- **Fix v4's rename bug:** when a porcelain line starts with `R` or `C` (rename / copy), `parse_porcelain_status` must emit BOTH the old and new path, not just the new one. v4 only emitted the destination, which works under `git add -A` but loses the old-name deletion under explicit `git add <paths>`. Add a unit test that asserts a rename produces two paths.
- Lift the rest of the v4 unit tests for both functions (status prefix parsing for M/A/D/??, quoted paths, scope partitioning, artifact-always-filtered invariant including with empty scope, leading `./` normalization).
- Wire into `lib.rs`: `pub mod scope;`.

#### Phase 2: Extend `ActionResult` with `dropped` field
**Model:** sonnet
- Update the `ActionResult` enum in `crates/agents/src/dispatch.rs` per the "ActionResult schema" section:
  - `Committed(String)` -> `Committed { sha: String, dropped: Vec<String> }`.
  - `NothingToCommit` -> `NothingToCommit { dropped: Vec<String> }`.
  - `BundleCreated(Bundle)` -> `BundleCreated { bundle: Bundle, dropped: Vec<String> }`.
- Update every match site for `ActionResult` (search across `crates/agents/`, `crates/loopr/`). Most sites only care about success vs. error so they will pattern-match `Committed { .. }` and ignore the new field.
- Update the iteration-history rendering in `implementer.rs` (the code path that builds the `combined_output` strings the LLM sees) to format a verbose note when `dropped` is non-empty: list the dropped paths, restate the Work's scope, and tell the agent its options. Empty `dropped` produces the existing terse summary.
- This phase ships first because Phase 3 depends on the new variant shapes.

#### Phase 3: Replace `git add -A` with `git commit --only` in `commit_changes` and `propose_bundle`
**Model:** sonnet
- Add two private helpers next to `is_working_tree_clean`:
  - `git_status_porcelain(path: &Path) -> Result<Vec<String>, DispatchError>` runs `git status --porcelain --untracked-files=all` (the `-uall` flag is mandatory; see the index-ownership section for why) and parses output via `scope::parse_porcelain_status`.
  - `git_diff_name_only(path: &Path, base_sha: &str) -> Result<Vec<String>, DispatchError>` runs `git diff --name-only <base>..HEAD` and returns the line-delimited paths.
- Rewrite `commit_changes` per the "Target shape" code sketch above: porcelain -> partition by `scope_files` -> if `in_scope` empty, return `NothingToCommit { dropped: out_of_scope }` -> else `git commit --only --message <msg> --no-gpg-sign -- <in_scope...>` and return `Committed { sha, dropped: out_of_scope }`. If `out_of_scope` is non-empty, also emit a `warn!` for log readers.
- Rewrite the staging block inside `propose_bundle` identically. After staging, populate `bundle.paths` from `git_diff_name_only(path, worktree.sha())` so the Bundle record reflects every path on the branch (not just the last staging step). Return `BundleCreated { bundle, dropped }`.
- Update the `#[instrument]` field set on both functions: post-record `in_scope_count`, `out_of_scope_count` via `Span::current().record(...)`. Record the scope size on entry as a span field.

#### Phase 4: Thread `&Work` through dispatch
**Model:** sonnet
- Change `dispatch_action`'s signature to take `work: &Work`. The `Work` type is already imported transitively via `domain` in this crate; add the explicit `use domain::Work;` if not already present.
- Update `crates/agents/src/implementer.rs` callers to pass the Work. Confirmed in scope at the call sites: `implementer.rs:280` and `:351` both have `work: &Work` as a function parameter (see `run_implementer` line 169).
- Update the `commit_changes` and `propose_bundle` action arms to read `work.files.as_slice()` and pass to the helpers.
- Update every existing `dispatch_action(...)` call in `crates/agents/src/dispatch/tests.rs` to pass a constructed `Work` with appropriate `files`. A helper `fn test_work(files: Vec<String>) -> Work` near the top of the test file keeps the rewrite mechanical.

#### Phase 5: Decomposer schema and prompt
**Model:** sonnet
- Add `files: Vec<String>` to `DecomposeChild` (default empty for backward compat with cached responses).
- Add the `files` property to the `submit_decomposition` JSON schema with a description that ties it to the staging allow-list.
- Add a sentence to `context/prompts/decompose/work/system.pmt` telling the LLM what `files` is and how it's used: "List the files (relative to the repo root) this Work is expected to create or modify. The implementer agent's commit will be restricted to these paths; out-of-scope edits will be flagged in the iteration result."
- Populate `work.files = child.files` in `decomposer/src/decompose.rs` after the existing assignments.

#### Phase 6: Tests
**Model:** sonnet
- Unit: `scope.rs` tests already lifted in Phase 1 (including the rename test that asserts `R old -> new` produces both paths).
- Seam: extend the dispatch crate tests to cover:
  - (a) `commit_changes` with non-empty scope drops out-of-scope files; the resulting `Committed.dropped` lists them.
  - (b) Empty scope still drops `.loopr/` artifacts (defensive parity).
  - (c) `commit_changes` does NOT commit a previously-staged out-of-scope file (the index-leak regression test): pre-stage `database.py` via `git add`, then call `commit_changes` with scope `["main.py"]`, assert HEAD's `git show --name-only` lists only `main.py`.
  - (d) `propose_bundle` populates `bundle.paths` from the full branch-vs-base diff including paths from earlier `commit_changes` actions.
  - (e) Untracked-directory enumeration: create `new_dir/main.py` (and parent dir), set scope `["new_dir/main.py"]`, assert it lands in the commit (this is the `--untracked-files=all` regression test).
- Integration: a new `agents/tests/scoped_staging.rs` that drives a real worktree end-to-end through `dispatch_action`.
- e2e: re-run `bin/e2e python-api` (with the existing `.venv/` `.gitignore` fix and the cargo `tail` removal) and assert all three Works integrate, not just `wk-87p62`.

#### Phase 7: Roadmap and instrumentation acceptance test updates
**Model:** sonnet
- Update `crates/agents/tests/instrumentation.rs::agents_smoke_spans_lifeguard_escalation` if it asserts on the old `commit_changes` field set or `ActionResult` shapes.
- Add a note to `docs/roadmap.md` under the appropriate stage marking scoped staging as shipped.

## Alternatives Considered

### Alternative 1: `.gitignore` only

- **Description:** Add `.venv/`, `node_modules/`, `target/`, `dist/`, etc. to every scaffold `.gitignore` in `bin/e2e` and rely on `git add -A`'s built-in `.gitignore` respect.
- **Pros:** Zero code change. Already partially done for `python-api` and `python-scraper`.
- **Cons:** Brittle. Every new build tool a future agent might invoke requires another scaffold edit. Fails open: if the scaffold misses a path, the agent commits garbage. Also does nothing for tracked-file modifications that should be out of scope (e.g., an agent editing `database.py` when its Work is `main.py`).
- **Why not chosen:** It's a symptom fix, not a structural one. The deeper fix is required regardless because tracked out-of-scope edits are a real risk and `.gitignore` cannot catch them.

### Alternative 2: Hardcoded artifact deny-list (no scope tags)

- **Description:** Filter `.loopr/`, `.venv/`, `node_modules/`, `target/`, `dist/`, `__pycache__/`, `.pytest_cache/` in `dispatch.rs`. Skip the decomposer/Work.files thread entirely.
- **Pros:** Single-crate change. No prompt change. No cache invalidation.
- **Cons:** Loopr would maintain a deny-list of every language ecosystem's build artifacts. Scope creep. Doesn't enforce per-Work file scope. Doesn't catch tracked-file edits outside scope.
- **Why not chosen:** Allow-listing per Work is more durable than deny-listing all known artifacts. The decomposer already names files in its prose; making it a structured field is a small lift.

### Alternative 3: Post-commit diff-size guard

- **Description:** After `commit_changes`, run `git diff --shortstat HEAD~1`; if more than N files or M lines, reject the commit (reset HEAD~1) and emit a correctable error to the agent.
- **Pros:** No staging logic change. Reactive guardrail catches blowups regardless of cause.
- **Cons:** Threshold is arbitrary; a legitimate scaffold-and-test commit can be large. Adds a reset step that's noisy in the agent's mental model. Still doesn't enforce scope; it only enforces size.
- **Why not chosen:** Reactive, not preventive. Treats the symptom not the cause.

### Alternative 4: Make the LLM emit `paths` per `commit_changes` action

- **Description:** Add a `paths: Vec<String>` field to the `CommitChanges` action variant; the implementer LLM names files at commit time.
- **Pros:** Maximum flexibility per commit.
- **Cons:** More tokens per iteration. The LLM has to remember which files it touched. Empirically (v4 evidence) this branch was rarely the one used; the scope-tag branch carried the load.
- **Why not chosen:** `Work.files` is sufficient. Per-action `paths` is a v6 question if scope-tags prove too coarse.

## Technical Considerations

### Dependencies

No new external crates. The change is plumbing across existing crates.

### Performance

`git status --porcelain` runs once per `commit_changes` and once per `propose_bundle` (only when the staging area is dirty). On an existing run that already calls `is_working_tree_clean` (which is `git status --porcelain` under the hood) we collapse the two calls into one read. Net: roughly even. The porcelain output for a typical worktree is well under 100KB; parsing is O(lines) and inconsequential.

### Security

None. No external surface area changes. The filter is a string-prefix match against a hardcoded constant.

### Testing Strategy

Three layers:

- **Unit** (`scope.rs`): the v4 test suite, lifted. Covers parsing, normalization, partitioning, and the artifact-always-filtered invariant.
- **Seam** (`dispatch.rs` tests): drive `commit_changes` and `propose_bundle` against a temp git repo (`tempfile::TempDir`), assert the staged set matches expectation under (a) non-empty scope, (b) empty scope, (c) artifact-only paths in the worktree, (d) mixed in-scope and out-of-scope changes.
- **e2e**: `bin/e2e python-api` is the canonical reproducer. Acceptance: all three Works integrate; `wk-ryqmy`'s bundle's diff contains `main.py`, not `.venv/`.

### Rollout Plan

Single commit on a feature branch. No coexistence flag, no gradual rollout - per the v5 working rule "no coexistence migrations." The change is internal to the daemon and affects only future runs. After merge, run the full `bin/e2e` matrix to confirm parity on Rust targets (where `Work.files` will likely be empty until Rust scaffolds are also updated; the empty-scope fallback covers this).

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM emits incomplete `files` list, blocking a legitimate commit | Med | Med | When `commit_changes` finds zero `in_scope` files, it returns `NothingToCommit { dropped: out_of_scope }`. The iteration-history rendering surfaces the dropped paths AND restates the scope to the LLM (Phase 2), so the agent has a parseable signal and can either edit in-scope files or emit `need_help`. The architect-round-1 concern that `tracing::warn!` is invisible to the LLM is addressed by the schema change. |
| Decomposer prompt change invalidates the system-prompt cache for the decomposer role | High | Low | The decomposer's system prompt already changes regularly during development; this is one more invalidation event. Cache will rebuild on first call and stabilize. The `agents` role caches are unaffected (different prompt). |
| Empty `Work.files` falls back to artifact-only filtering, which doesn't catch out-of-scope tracked edits | Med | Low | Acceptable transition state. Once decomposer + scaffolds emit `files` consistently, this path is exercised only by legacy / partially-migrated Works. The failure mode of empty-scope is the current behavior, not a regression. |
| `propose_bundle` populating `bundle.paths` changes reviewer/integrator behavior unexpectedly | Low | Low | Both downstream consumers (`reviewer.rs:143` `git_show` filtering, `integrator/classify.rs:35` collision detection) already read `bundle.paths` and silently treat empty as "match nothing." Populating it activates intended behavior they were already coded for. The change is "turn on a feature that was wired but starved of inputs," not a behavioral surprise. v4 confirms this is the intended shape. |
| Reviewer's filtered diff hides a legitimate out-of-scope edit the agent made (e.g., a bug fix in a shared file) | Low | Med | The diff is filtered, but the staged set was filtered identically, so an out-of-scope edit was never reached the commit. The agent receives the dropped path list in `dropped`; if it intended to commit them, it must use `need_help` to request scope expansion. |
| Agent stages a path via `bash: git add <out-of-scope>`, leading to silent commit of out-of-scope content | Was-blocking | High | RESOLVED by index-ownership decision: `git commit --only -- <paths>` ignores stale index entries. Pre-staged out-of-scope files persist in the index but never reach a commit. Phase 6 adds an explicit regression test (test (c) above). |
| Untracked new directory hits `?? new_dir/` rollup, scope match fails | Was-blocking | Med | RESOLVED: helper passes `--untracked-files=all` so each new file enumerates individually. Phase 6 adds the regression test (test (e) above). |
| Massive scope (thousands of paths) hits OS argv limit (`E2BIG`) | Low | Med | Theoretical at our scale (a Work with thousands of files would fail decomposition validation first). If observed, batch the `git commit --only -- ...` invocation across multiple sub-commits or use `git update-index --add --stdin`. Tracked as an open question, not blocking. |
| A worktree with both committed-but-untouched-by-this-action edits and new in-scope edits stages too narrowly | Low | Low | `commit_changes` operates on the porcelain output, which includes ALL dirty paths regardless of which iteration created them. The filter applies uniformly. No special case. |

## Open Questions

- [ ] **Scope match semantics: exact path, prefix, or glob?** v4 was exact-path only, so `scope_files = ["src/"]` would NOT match `src/main.rs`. This forces the decomposer to enumerate every file. Three options:
  - (a) **Exact only** (v4 behavior, lowest implementation cost). Decomposer must list every file. Risk: a Work that creates a new file the decomposer didn't predict will hit `NothingToCommit` and need a `need_help` cycle. The new `dropped` feedback in `ActionResult` makes this failure mode legible to the agent.
  - (b) **Prefix match** (`scope_files = ["src/"]` matches `src/main.rs`). Cheap to implement; one `starts_with` check. Risk: too coarse - `["src/"]` would match `src/anything.rs` including out-of-scope files.
  - (c) **Glob** (`scope_files = ["src/*.rs"]`). Most expressive; needs a glob crate or implementation.
  - **Recommendation: start with (a) exact-path,** matching v4. If a real Work hits the "decomposer didn't predict the file" failure mode in practice, escalate to (c) glob. Avoid (b) - the false-positive risk is worse than (a)'s false-negative (which surfaces as a clean `NothingToCommit { dropped }` rather than silent wrong commits).
- [ ] **Noop bundles' `bundle.paths`.** When the agent emits `Done { message }` without a commit, the resulting noop Bundle has `paths = vec![]`. The reviewer's `read_file_contents` (line 149 in `reviewer.rs`) reads `bundle.paths` to show the reviewer the file contents being claimed-as-already-correct. With empty paths the reviewer sees nothing. v4 had a separate `noop_paths` field on the action for this. Out of scope for this design but flag for a follow-up: noop bundles probably want `bundle.paths = work.files` as a default.
- [ ] **`E2BIG` on massive scopes.** A Work decomposed to thousands of files would hit the OS argv limit when invoking `git commit --only -- <paths...>`. Theoretical at current scale; if observed in practice, switch the staging step to `git update-index --add --stdin` fed via the `--null` form. Tracking here so the failure mode has a known answer if it lands.
- [ ] Should the reviewer be told the scope set so it can flag scope violations explicitly? Defer to a separate design doc; the immediate scope of this work is staging.
- [ ] Should the Rust scaffolds (`rust-version`, `rust-cli`) populate `Work.files` in their PRDs, or rely on the decomposer to derive them from the prose? Recommendation: rely on the decomposer; the PRD prose already names files.

## Architect Round 1 Findings

The Architect persona reviewed this design on 2026-04-26 and identified three blocking issues plus one theoretical concern. All blocking issues are addressed by revisions in the body of this document.

| # | Finding | Status | Resolution |
|---|---------|--------|------------|
| 1 | **Index-leak via `git add` + `git commit`** - if the agent staged out-of-scope paths via `bash: git add ...` before our `commit_changes`, a naive `git add -- <in_scope> && git commit` would fold those staged paths into the commit because `git commit` commits the entire index by default. | Resolved | Switched to `git commit --only -- <paths>` which ignores stale index entries. See "Index ownership" section. |
| 2 | **`tracing::warn!` is invisible to the LLM** - the agent had no signal when its files were filtered out of scope, leading to deterministic stuck-loop escalations as the agent retries the same out-of-scope edit. | Resolved | Extended `ActionResult::Committed`, `NothingToCommit`, and `BundleCreated` with a `dropped: Vec<String>` field. `implementer.rs` renders the dropped set as a structured note in the iteration history. See "ActionResult schema" section. |
| 3 | **Untracked-directory rollup** - default `git status --porcelain` collapses new directories to `?? new_dir/`, which exact-path scope matching would treat as out-of-scope, blocking creation of new files in new directories. | Resolved | `git_status_porcelain` helper passes `--untracked-files=all` so individual files enumerate. See "Index ownership" section and Phase 3. |
| 4 | **`E2BIG` on massive scopes** - thousands of paths in a single `git commit --only -- ...` argv could exceed the OS limit. | Tracked, not blocking | Listed in Open Questions. Theoretical at current scale; mitigation strategy noted (`git update-index --add --stdin`). |

The Architect's "hardest question" - whether already-staged renames would still leak under a filtered `git add` - was answered structurally by the `--only` decision: `git commit --only -- old new` either covers both sides of the rename or neither, regardless of prior index state. Documented in "Rename and deletion handling".

## References

- v4 implementation: `~/repos/scottidler/loopr-v4/src/agents/executor/action/scope.rs`, `~/repos/scottidler/loopr-v4/src/agents/executor/action/file.rs:188-239`
- v3 baseline (the regression target): `~/repos/scottidler/loopr/src/agents/executor.rs:509-529`
- v5 current state: `crates/agents/src/dispatch.rs:114-122` (`commit_changes`), `:154-177` (`propose_bundle`)
- The 2026-04-26 e2e failure that motivated this: `python-api` run at `/tmp/loopr/e2e/python-api/20260426-122430` (events log preserved under `~/.local/share/loopr/sessions/20260426-122430-2/`)
- v5 working rule on coexistence: `CLAUDE.md` "No coexistence migrations"
