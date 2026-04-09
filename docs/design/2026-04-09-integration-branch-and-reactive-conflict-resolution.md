# Design Document: Integration Branch Model, Reactive Conflict Resolution, and LLM Conversation Capture

**Author:** Scott A. Idler
**Date:** 2026-04-09
**Status:** Implemented
**Review Passes Completed:** 5/5 + architectural audit

## Summary

Replace the current direct-to-HEAD merge model with a per-Plan integration branch, add a `bundle.merged` event that triggers implementer rebase, remove preventative same-file scheduling constraints from decomposer prompts in favor of reactive conflict resolution at integration time, strip the dead `files` field from the Work domain model and all prompts, and add log-level-gated LLM conversation capture as standalone log files alongside `loopr.log`.

## Problem Statement

### Background

Loopr's orchestration model decomposes Plans into a hierarchy (Plan -> Spec -> Phase -> Work) and executes Work items in parallel via isolated git worktrees. Each implementer works on an `agent/<work_id>` branch, produces a Bundle, and the Integrator merges bundle branches into the current HEAD using `git merge --no-ff`.

The decomposer prompt (`prompts/decompose/work.pmt`) contains a "Same-File Rule" that forces sequential dependencies between Work items targeting the same file, and a "3+ chain collapse" rule in the "Parallelism" section. The Work domain model carries a `files: Vec<String>` field populated by the decomposer. These were introduced in `2026-04-08-e2e-parallelism-and-recovery.md` to prevent merge conflicts.

The current model has three problems:

1. **Prompt contradiction**: The Parallelism section says "A dependency chain of 3+ items (A -> B -> C) is a signal you have over-decomposed. Collapse the chain into one work item." The Same-File Rule directly below it demonstrates a 3-item chain (Fixtures -> Health tests -> CRUD tests). The LLM follows the example and ignores the collapse rule.

2. **Preventative constraints are the wrong model**: Forcing the decomposer to predict file conflicts at planning time is brittle. The LLM often gets it wrong (empty `files: []` arrays, or incorrect predictions), and the dependency chains it creates to avoid conflicts serialize work that could run in parallel. The correct place to resolve conflicts is at integration time, when the actual diffs are known.

3. **No conversation capture**: The full LLM request/response bodies are never written to disk. `AgentLlmClient` logs only lengths (`system_prompt_len=N, user_msg_len=N`). When decomposition produces bad output or an implementer goes off-track, there is no replay capability - the conversation is lost.

4. **Integration merges to HEAD implicitly**: `merge_bundle_branches()` runs `git merge --no-ff <branch>` into whatever branch is currently checked out. There is no dedicated integration branch, so merges land directly on main. This means a failed validation run on a Tick leaves broken commits on main that must be manually cleaned up.

### Problem

The decomposer produces serial chains where parallel execution is possible, the `files` field is dead data the execution engine no longer uses for routing, there is no observability into LLM conversations, and the integration model lacks branch isolation.

### Goals

- Remove prompt contradictions and dead constraints (Same-File Rule, 3+ collapse rule, `files` field)
- Integration branch per Plan isolating in-progress work from main
- `bundle.merged` event enabling implementer rebase against updated integration branch
- Implementer-internal rebase states (not exposed to Work FSM)
- Config-gated rebase-on-merge behavior with max-rebase-lag safety limit
- LLM conversation capture as standalone log files, gated by log level (not INFO)

### Non-Goals

- Concurrent Plan execution (acknowledged risk, deferred - two Plans merging to main simultaneously is a problem for later)
- TUI changes for rebase state visibility (TUI is languishing; will be addressed separately)
- Changing the Work FSM (`Draft -> Pending -> Ready -> InProgress -> ... -> Done/Abandoned` stays)
- Changing the Bundle FSM
- Semantic 3-way merge by the Integrator (Integrator remains deterministic, no LLM)

## Proposed Solution

### Part 1: Prompt and Schema Cleanup

#### 1a. Remove Same-File Rule from decomposer prompt

**File:** `prompts/decompose/work.pmt`

Delete lines 51-64 (the entire `## Same-File Rule` section):

```
## Same-File Rule

If two or more Work items target the SAME file, they MUST have sequential dependencies
between them. The first item creates the file; each subsequent item depends on the
previous one. This is non-negotiable - parallel writers on the same file produce
unresolvable merge conflicts.

Order by: scaffolding/structure first, then content additions, then tests that
import from earlier content.

Example: if three Works all write to `test_api.py`:
  - Work A (Fixtures): files=["tests/conftest.py", "tests/test_api.py"], deps=[]
  - Work B (Health tests): files=["tests/test_api.py"], deps=["Fixtures"]
  - Work C (CRUD tests): files=["tests/test_api.py"], deps=["Health tests"]
```

**Rationale:** Conflict resolution moves to the Integrator. The decomposer should produce maximally parallel work items.

#### 1b. Remove 3+ chain collapse rule from Parallelism section

**File:** `prompts/decompose/work.pmt`

In the `## Parallelism` section, remove:
```
- A dependency chain of 3+ items (A -> B -> C) is a signal you have
  over-decomposed. Collapse the chain into one work item.
```

And remove:
```
- Work items that touch different files can always run in parallel. Declare
  files accurately so the orchestrator can schedule them concurrently.
```

Replace the Parallelism section with:

```
## Parallelism

Work items are discrete chunks that can be built independently and in parallel.
This is their primary design purpose. When decomposing:

- Most work items should have NO dependencies. If an implementer needs the
  output of another work item to start, that is a dependency. If they just
  need to know the interface contract (function signatures, API shape), that
  is NOT a dependency - the contract is already defined in the Spec.
- Prefer fan-out: many independent Work items, NOT linear chains.
  Two independent items beat five dependent ones.
- If multiple Work items write to the same file, that is fine. The Integrator
  handles merge conflicts at integration time. Do NOT add dependencies
  between Work items solely because they touch the same file.
```

#### 1c. Remove `files` field from Work domain model

**File:** `src/domain/work.rs`

Remove:
```rust
#[serde(alias = "resource_tags")]
pub files: Vec<String>,
```

Remove `files: Vec::new()` from `Work::new()`.
Remove `m.push(("files".into(), FmValue::List(self.files.clone())));` from `doc_frontmatter()`.
Update tests that reference `files`.

**File:** `prompts/decompose/work.pmt`

Remove from the JSON schema example:
```json
"files": ["src/main.py", "tests/test_main.py"]
```

Remove Rule 7:
```
7. Each Work must include a "files" array listing every file it will create or modify.
   The orchestrator uses this to detect conflicts and schedule non-overlapping items concurrently.
```

Also remove from Rule 2:
```
The ONLY valid reason for a dependency is a same-file conflict.
```

Replace with:
```
2. Dependencies are same-phase Work titles only. Do NOT reference Works from other Phases.
   The ONLY valid reason for a dependency is when a Work literally cannot compile or
   test without another Work's output being present in the repo.
```

**File:** `src/decomposer.rs`

Remove from the tool schema (lines 231-235):
```json
"files": {
    "type": "array",
    "items": {"type": "string"},
    "description": "Files this work item will create or modify (relative paths)"
}
```

**File:** `prompts/generation-work.pmt`

Remove:
```
- Include files as relative file paths that will be modified (e.g., ["src/auth/jwt.rs", "src/auth/mod.rs"])
```

Remove:
```
- If two Works touch DIFFERENT files, they MUST have NO dependency between them.
```

**Backward compatibility:** Existing JSONL records with `files` or `resource_tags` will still deserialize thanks to serde's `default` behavior for missing fields. Since we're removing the field entirely, old records that include `files` will get a deserialization error unless we add `#[serde(deny_unknown_fields)]` - which we don't. Serde's default behavior ignores unknown fields, so old records with `files` present will silently skip it. No migration needed.

**Downstream impact on `classify_conflict()` (CRITICAL):** The original draft incorrectly claimed this function uses `bundle.touched_paths`. Empirical verification shows `classify_conflict()` at `src/agents/integrator.rs:1209` iterates over `work.files`:

```rust
for file in &work.files {
    if let Some(first_work) = file_to_work.get(file) { ... }
}
```

Removing `work.files` without rewriting this function would silently disable conflict detection, causing the Integrator to merge conflicting bundles blindly.

**Mandate:** Rewrite `classify_conflict()` to use `bundle.touched_paths` instead of `work.files`. The `touched_paths` field on Bundle is populated from actual git diff output (the real files changed in the worktree), making it a more accurate data source than the LLM-predicted `files` field. This is not just a migration - it's an improvement: `touched_paths` reflects reality, `files` reflected LLM guesses.

```rust
// BEFORE (work.files - LLM predictions, often empty):
for file in &work.files { ... }

// AFTER (bundle.touched_paths - actual git diff output):
for file in &bundle.touched_paths { ... }
```

The function signature stays the same. Only the inner loop changes from iterating `work.files` to iterating `bundle.touched_paths`, eliminating the work lookup entirely.

**Downstream impact on `loose_files` in Bundle:** The Bundle struct has a `loose_files: Vec<String>` field described as "Files modified in the worktree but excluded from the bundle because they fall outside the Work's files scope." This field is semantically dependent on Work's `files` - without a file scope, there is no concept of "loose" files. Remove `loose_files` from Bundle as well. It is dead data once `files` is gone.

**File:** `src/domain/bundle.rs`

Remove:
```rust
#[serde(default)]
pub loose_files: Vec<String>,
```

Remove `loose_files: Vec::new()` from `Bundle::new()`. Serde's `#[serde(default)]` was already on this field, so old JSONL records with `loose_files` present will have it silently ignored during deserialization.

#### 1d. Remove `files` references from generation-work.pmt

**File:** `prompts/generation-work.pmt`

The generation prompt also references `files`. Remove all file-list instructions and the dependency rule that references files.

### Part 2: Integration Branch Model

#### 2a. Integration branch per Plan

**Invariant:** An integration branch is strictly bound to the lifecycle of a Plan. It is spawned when the Plan transitions to its execution state and merged to main when the Plan completes successfully.

**Branch naming:** `integration/<plan_id>` (e.g., `integration/pl-abc12`)

**Lifecycle:**

1. **Creation**: When the Coordinator starts executing a Plan, create the integration branch from main's current HEAD:
   ```
   git checkout -b integration/<plan_id> main
   ```

2. **Worktree base**: All implementer worktrees branch from the integration branch's HEAD, not from main:
   ```
   git worktree add .worktrees/<work_id> -b agent/<work_id> integration/<plan_id>
   ```

3. **Bundle merge target**: The Integrator's `merge_bundle_branches()` merges into the integration branch, not main:
   ```
   git checkout integration/<plan_id>
   git merge --no-ff agent/<work_id>
   ```

4. **Plan completion**: When all Work items are Done and the Plan is complete, the integration branch is merged to main:
   ```
   git checkout main
   git merge --no-ff integration/<plan_id> -m "Merge Plan <plan_id>: <plan_title>"
   ```

5. **Plan failure/abandonment**: If the Plan fails or is abandoned, the integration branch is deleted. Main is never touched:
   ```
   git branch -D integration/<plan_id>
   ```

**Code changes:**

**File:** `src/agents/integrator.rs`

**Critical context gap (identified in Gemini audit):** The Integrator currently has no `plan_id` context. `AgentContext` does not store a `plan_id`, and the Integrator polls for Accepted bundles globally via `stores.read_bundles()`. With per-Plan integration branches, the Integrator must know which Plan each bundle belongs to.

**Resolution - plan_id discovery via parent chain:**

Each Bundle has a `work_id`. Each Work has a `parent_id` (Phase). Each Phase has a `parent_id` (Spec). Each Spec has a `parent_id` (Plan). The Integrator traverses this chain to discover the `plan_id` for each bundle:

```rust
fn resolve_plan_id(stores: &Stores, work_id: &str) -> Option<String> {
    let works = stores.read_works().ok()?;
    let work = works.get(work_id)?;
    let phases = stores.read_phases().ok()?;
    let phase = phases.get(work.parent_id.as_str())?;
    let specs = stores.read_specs().ok()?;
    let spec = specs.get(phase.parent_id.as_str())?;
    Some(spec.parent_id.clone()) // Plan ID
}
```

**Tick is now Plan-scoped:** With integration branches, a Tick is a snapshot of a specific Plan's integration branch, not a global snapshot of main. Add `plan_id: String` to the Tick domain model. The Integrator's `run_cycle()` must:

1. Collect all Accepted bundles
2. Partition them by `plan_id` (via `resolve_plan_id()`)
3. For each `plan_id` group: create a Tick scoped to that Plan, checkout `integration/<plan_id>`, merge the group's bundle branches, validate, publish or fail

This ensures bundles from different Plans never cross-contaminate each other's integration branches.

`merge_bundle_branches()` changes:
- Accept `integration_branch: &str` parameter
- Run `git checkout <integration_branch>` before the merge loop
- Verify the branch exists via `git rev-parse --verify` before checkout (see Fix #4 below)
- On completion, remain on the integration branch (don't switch back to main)

**File:** `src/worktree/manager.rs`

`create_branch()` currently accepts `base_ref: &str`. The caller (Coordinator) must pass the integration branch name instead of a Tick SHA. This is a calling-convention change, not an API change - `base_ref` already accepts any git ref.

`refresh()` must be updated to abort on rebase failure. Currently it returns an error but leaves the worktree in a half-rebased state (`git rebase` started but didn't complete). Add `git rebase --abort` in the error path before returning the error. Without this, every subsequent git operation in the worktree will fail with "rebase in progress."

**File:** `src/agents/coordinator/` (the coordinator FSM)

When a Plan enters execution:
- Create integration branch `integration/<plan_id>` from main HEAD
- The branch name is deterministically derived from the plan_id (`format!("integration/{}", plan_id)`), so no storage needed - any component that has the plan_id can compute it
- Pass this branch name as `base_ref` when spawning implementer worktrees
- The Integrator receives the plan_id via its existing context and uses it to derive the integration branch name

When a Plan completes:
- Merge integration branch to main
- Delete integration branch
- Delete all `agent/*` branches for the Plan's Work items

When a Plan fails/is abandoned:
- Delete integration branch (and all agent branches)
- Main is untouched

#### 2b. Failed validation rollback

When the Integrator merges bundle branches into the integration branch and validation subsequently fails, the broken commits must be rolled back. Otherwise the integration branch carries broken code, and new implementer worktrees branching from it will start from a broken base.

**Before checkout**, verify the integration branch exists:
```
git rev-parse --verify integration/<plan_id>
```

If the branch is missing (deleted manually, daemon crashed before Coordinator created it, etc.), the Integrator must reject all bundles for that Plan's Tick and fail the Tick gracefully. Do not panic or loop - emit a `reconciliation.failed` event and move on. The Coordinator will detect the missing branch and recreate it.

**Before the merge loop**, record the current integration branch HEAD:
```
pre_merge_sha=$(git rev-parse HEAD)
```

**If validation fails**, reset the integration branch:
```
git reset --hard <pre_merge_sha>
```

This keeps the integration branch in a known-good state at all times: it only advances when validation passes.

The current code already handles merge failures (via `git merge --abort`), but validation failures after a successful merge are a new concern unique to the integration branch model. On the current direct-to-main model, validation failures leave broken commits on main - this is a known problem that the integration branch model fixes.

#### 2c. Main advancement policy

**Default:** Main advances only at Plan completion. The integration branch accumulates all intermediate merges.

**Spec-level advancement (future option):** If a Spec within a Plan is provably independent (no shared files or dependencies with other Specs), its Work can be merged to main at Spec completion. This requires:
- All Work items in the Spec are Done
- Validation passes on the integration branch after the Spec's final merge
- No other Spec in the Plan has unresolved dependencies on this Spec

This is deferred to future work. For now, main advances only at Plan completion.

### Part 3: bundle.merged Event and Implementer Rebase Protocol

#### 3a. bundle.merged event

**Timing:** The event fires AFTER validation passes and the Tick is published - not after the git merge but before validation. This is critical: implementers must only rebase against a validated integration branch. If they rebase against a post-merge pre-validation state, they could be rebasing onto broken code.

**Relationship to `tick.published`:** The existing `tick.published` event already fires at the right moment and carries `tick_id` and `sha`. We could reuse it, but `bundle.merged` is semantically clearer for the rebase use case and carries the list of merged bundle IDs (useful for debugging). Both events fire at the same point in the Integrator cycle. The implementer listens for `bundle.merged` specifically for rebase; the existing `tick.published` handler for staleness detection remains unchanged.

**File:** `src/ipc/protocol.rs`

Add a new event constructor:

```rust
pub fn bundle_merged(tick_id: &str, integration_sha: &str, merged_bundle_ids: &[String]) -> Self {
    Self::new(
        "bundle.merged",
        serde_json::json!({
            "tick_id": tick_id,
            "integration_sha": integration_sha,
            "merged_bundle_ids": merged_bundle_ids,
        }),
    )
}
```

**File:** `src/agents/integrator.rs`

After a Tick is published successfully and bundles transition to Merged, emit the event:

```rust
let _ = self.ctx.event_tx.send(DaemonEvent::bundle_merged(
    &tick_id,
    &integration_sha,
    &merged_bundle_ids,
));
```

#### 3b. Implementer rebase protocol (internal actor states)

**Critical design decision from Gemini audit:** `Rebasing`, `Conflicted`, and `Queued` are NOT Work FSM states. They are internal to the Implementer actor. To the global system, the Work item remains `InProgress` throughout. The Implementer process internally transitions through these states without broadcasting FSM updates to the coordinator.

**Implementer internal states:**

```
Working -> Rebasing     (bundle.merged event received between iterations)
Rebasing -> Working     (rebase clean)
Rebasing -> Conflicted  (rebase has conflicts)
Conflicted -> Working   (implementer resolved conflicts)
Conflicted -> Yielded   (gave up - yields back to pool)
```

When `Yielded`: The Work item transitions globally from `InProgress -> Ready` (via Coordinator override), carrying `{ unsatisfied_ac }` context so the next implementer doesn't redo completed work. The `attempt_count` increments.

**Implementation in `src/agents/implementer.rs`:**

Between agentic iterations (not mid-LLM-call), the implementer drains the broadcast channel for `bundle.merged` events (same pattern as existing `tick.published` staleness detection):

```rust
fn drain_bundle_merged(&mut self) -> Option<String> {
    let mut latest_sha = None;
    while let Ok(event) = self.event_rx.try_recv() {
        if event.event_type == "bundle.merged" {
            if let Some(sha) = event.data.get("integration_sha").and_then(|v| v.as_str()) {
                latest_sha = Some(sha.to_string());
            }
        }
    }
    latest_sha
}
```

At the top of each iteration, after draining:

```rust
if let Some(new_sha) = self.drain_bundle_merged() {
    if self.config.rebase_on_merge {
        match self.worktree_manager.refresh(&self.work_id, &new_sha) {
            Ok(()) => {
                self.rebase_lag = 0; // reset on success
                self.inject_rebase_note("Rebased to integration branch HEAD");
            }
            Err(e) => {
                // Rebase conflict - abort to leave worktree in clean state.
                // WorktreeManager.refresh() must be updated to run
                // `git rebase --abort` when rebase fails, so the worktree
                // isn't left in a half-rebased state that blocks all
                // subsequent git operations.
                if self.rebase_lag >= self.config.max_rebase_lag {
                    // Too far behind - yield
                    return Err(eyre!("rebase lag exceeded max ({}), yielding", self.config.max_rebase_lag));
                }
                self.rebase_lag += 1;
                // Note: rebase_lag resets to 0 on successful rebase
                self.inject_conflict_note(&format!("Rebase conflict: {}. Continuing from pre-rebase state.", e));
            }
        }
    }
}
```

**Debouncing:** Multiple `bundle.merged` events arriving in rapid succession are collapsed by `drain_bundle_merged()` - only the latest SHA matters. The implementer rebases once per iteration, not per event.

**Event channel unification (MANDATORY):** The implementer already drains `tick.published` events from the same `broadcast::Receiver` via `drain_tick_published()`. Because `try_recv()` consumes messages from the channel, calling two separate drain functions sequentially will drop events of the type the first function doesn't handle. This is not a recommendation - it is a correctness requirement.

Replace `drain_tick_published()` and the proposed `drain_bundle_merged()` with a single `drain_events()` method. It must match on event type in one pass and return a unified struct:

```rust
struct DrainedEvents {
    latest_tick_sha: Option<String>,       // from tick.published
    latest_integration_sha: Option<String>, // from bundle.merged
}

fn drain_events(&mut self) -> DrainedEvents {
    let mut result = DrainedEvents { latest_tick_sha: None, latest_integration_sha: None };
    while let Ok(event) = self.event_rx.try_recv() {
        match event.event_type.as_str() {
            "tick.published" => {
                if let Some(sha) = event.data.get("sha").and_then(|v| v.as_str()) {
                    result.latest_tick_sha = Some(sha.to_string());
                }
            }
            "bundle.merged" => {
                if let Some(sha) = event.data.get("integration_sha").and_then(|v| v.as_str()) {
                    result.latest_integration_sha = Some(sha.to_string());
                }
            }
            _ => {} // discard other event types
        }
    }
    result
}
```

The existing staleness detection logic moves into the caller, which checks `result.latest_tick_sha`. The new rebase logic checks `result.latest_integration_sha`. Both are handled from a single drain pass.

#### 3c. Queued bundle rebase

When a bundle is in the Integrator's internal queue (Work status is `Integrating`, bundle is `Accepted`) and a `bundle.merged` event fires, the Integrator rebases the queued bundle's branch against the new integration HEAD before attempting the merge:

```rust
// Before merging, rebase the bundle branch onto the integration branch HEAD.
// git rebase <upstream> <branch> replays <branch>'s commits that aren't
// on <upstream> onto <upstream>'s tip.
let output = Command::new("git")
    .args(["rebase", &integration_branch, &bundle_branch])
    .current_dir(&repo_path)
    .output()?;

if !output.status.success() {
    // Rebase failed - abort, reject the bundle, reset Work to Ready.
    // The implementer will get a fresh start with current file state.
    let _ = Command::new("git")
        .args(["rebase", "--abort"])
        .current_dir(&repo_path)
        .output();
}
```

If the rebase succeeds, the merge proceeds as a fast-forward. If it fails, the bundle is Rejected, the Work returns to Ready with `attempt_count` incremented and `unsatisfied_ac` carried forward.

#### 3d. Post-rebase AC check

After an implementer rebases its worktree, there is a chance the new integration HEAD already satisfies some or all of the Work's acceptance criteria (a prior bundle covered the same ground):

- **All ACs pass post-rebase:** The Work can be marked Done without re-implementing. The implementer proposes a noop bundle.
- **Partial satisfaction:** The implementer implements only the remaining delta. Its LLM context already includes the current file state.
- **No ACs satisfied:** Normal implementation continues.

This check happens naturally in the implementer's existing iteration loop - the LLM reads the current file state and compares against ACs. No special logic needed beyond injecting a rebase note into the conversation context.

### Part 4: Configuration

#### 4a. Integrator config additions

**File:** `src/config.rs`

Add to `IntegratorConfig`:

```rust
pub struct IntegratorConfig {
    pub validation_commands: Vec<String>,
    pub interval_secs: u64,
    pub enabled: bool,
    pub session_timeout_secs: Option<u64>,
    // New fields:
    pub rebase_on_merge: bool,       // default: true
    pub max_rebase_lag: u32,         // default: 5
}
```

**YAML:**

```yaml
integrator:
  rebase-on-merge: true    # enable bundle.merged -> rebase propagation
  max-rebase-lag: 5        # abandon worktree after N consecutive rebase failures
```

When `rebase-on-merge: false`, the system reverts to current behavior: conflicts surface at merge time, the Integrator handles them (reject + retry), and implementers are never interrupted mid-work.

### Part 5: LLM Conversation Capture

#### 5a. Design

LLM conversations are captured as standalone log files alongside `loopr.log` in the session directory. Each domain action that calls the LLM gets its own log file, named by the domain object that triggered it.

**Log file location:**

```
~/.local/share/loopr/sessions/{session_id}/
  loopr.log                              # existing - operational log
  reconciliation.log                     # existing - reconciler audit
  conversations/                         # NEW - LLM conversation logs
    decompose-{parent_id}.log            # decomposer call at each level
    implement-{work_id}.log              # implementer agentic loop
    integrate-{tick_id}.log              # integrator (currently no LLM, future-proof)
    evaluate-{parent_id}.log             # coverage evaluator
    validate-{doc_id}.log                # doc validator
    tier-gate-{plan_id}.log              # tier gate classification
    chat-{session_id}.log                # interactive chat sessions
```

**Format:** Plain text, human-readable. Each request/response pair is separated by a delimiter:

```
=== REQUEST 2026-04-09T14:32:01.123Z ===
[system]
<system prompt text>

[user]
<user message text>

[tools]
<tool definitions if present, JSON>

=== RESPONSE 2026-04-09T14:32:04.567Z ===
<full response text or content blocks>

=== REQUEST 2026-04-09T14:32:05.100Z ===
...
```

#### 5b. Log level gating

Conversation logs are only created when the effective log level is DEBUG, TRACE, WARN, or ERROR - NOT INFO.

```rust
fn should_capture_conversations(level: LevelFilter) -> bool {
    level != LevelFilter::Info && level != LevelFilter::Off
}
```

**Rationale:** INFO is the production default. Conversation capture generates large files (megabytes per work item) and should be opt-in. Conversation logs are created at DEBUG, TRACE, WARN, and ERROR levels - any level except INFO and Off. The conversation log files are standalone (not part of the tracing subscriber), so they write full request/response pairs regardless of the tracing filter level. The log-level check gates **file creation**, not content filtering.

**Implementation:** Check the resolved log level at session setup time. If it's INFO or Off, skip creating the `conversations/` directory. If it's any other level, create it and pass a `conversation_log_dir` path to the LLM client.

#### 5c. Implementation

**File:** `src/lib.rs`

In `setup_logging()`, when `session_id` is provided and log level is not INFO:
- Create `{session_dir}/conversations/` directory
- Store the path in a shared location (e.g., return it alongside `LogHandle`)

**File:** `src/agents/llm_client.rs`

Add a `conversation_log_dir: Option<PathBuf>` field to `AgentLlmClient`. When set:

In `call_streaming_with_messages()`:
- Before the HTTP call, append the request to the conversation log
- After response completes, append the response

In `complete()` (tool-use path):
- Same pattern: request body before, response content blocks after

The log file path is derived from context: the LLM client knows its `session_id` and the caller provides the domain context (work_id, plan_id, etc.) when constructing the client.

**File append pattern:**

```rust
fn append_to_conversation_log(&self, content: &str) {
    if let Some(ref dir) = self.conversation_log_dir {
        let path = dir.join(&self.conversation_log_name);
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = std::io::Write::write_all(&mut file, content.as_bytes());
        }
    }
}
```

### Part 6: Concurrent Plans (Deferred)

If two Plans run simultaneously, each has its own `integration/<plan_id>` branch. When both complete, they both try to merge to main. The second merge may conflict.

This is the same problem as Work-level conflicts, one level up. The solution is also the same: the second Plan's merge to main detects the conflict, and the system either:
- Retries the Plan's final merge after rebasing the integration branch onto the new main
- Fails the Plan and requires redecomposition from the new main state

This is acknowledged as a real future scenario but deferred because:
- The system currently runs one Plan at a time
- The integration branch model does not make this worse (today both Plans would merge directly to main with the same conflict risk)
- The rebase infrastructure built here applies directly to the Plan-level problem later

## Alternatives Considered

### Alternative 1: Semantic 3-way merge by Integrator (LLM-powered)

- **Description:** Instead of `git merge`, have the Integrator use an LLM to perform semantic 3-way merges when git conflicts occur.
- **Pros:** Could resolve conflicts that git cannot (e.g., two functions added to the same file in different locations).
- **Cons:** Breaks the Integrator's deterministic property (no LLM). Adds latency and cost to every conflicted merge. Hard to verify correctness.
- **Why not chosen:** The Implementer is the right place to resolve conflicts - it has full context about what it was trying to do and why. The Integrator should stay deterministic.

### Alternative 2: Keep Same-File Rule, fix the example

- **Description:** Fix the prompt contradiction by changing the Same-File Rule example to collapse all same-file writes into one work item.
- **Pros:** Simpler change. No new infrastructure needed.
- **Cons:** Still forces the decomposer to predict file conflicts at planning time. Still serializes work that could run in parallel. The "files" field is still dead data the LLM fills incorrectly.
- **Why not chosen:** The fundamental model (preventative constraints) is wrong. Reactive conflict resolution at integration time is architecturally correct.

### Alternative 3: debug! logging for LLM capture

- **Description:** Add `debug!()` calls that serialize the full request/response JSON into the standard tracing subscriber.
- **Pros:** No new infrastructure - uses existing logging.
- **Cons:** Destroys the utility of `loopr.log`. Multi-megabyte JSON payloads interleaved with operational logs make it impossible to debug daemon issues. Log rotation thrashing. IO bottleneck on a single file.
- **Why not chosen:** Gemini's architectural audit correctly identified this as log poisoning. Telemetry data and operational logs must remain segregated.

### Alternative 4: Per-agent JSONL files

- **Description:** Each agent session writes a `{session_id}.jsonl` file with structured request/response records.
- **Pros:** Machine-parseable. Good for automated analysis.
- **Cons:** Not human-readable for quick debugging. JSONL is the right format for heterogeneous records in a shared store, but these are linear conversations owned by one domain object.
- **Why not chosen:** Plain text logs per domain object are simpler, greppable, and directly navigable. A developer investigating "why did wk-abc12's implementer go off-track?" opens one file and reads a conversation.

## Technical Considerations

### Dependencies

- No new crate dependencies
- Existing: `tokio::sync::broadcast` for event propagation, `std::process::Command` for git operations, `tracing` for log-level resolution

### Performance

- Integration branch operations are cheap git operations (checkout, merge, branch create/delete)
- Rebase between iterations adds one `git rebase` call per `bundle.merged` event (debounced to at most one per iteration)
- Conversation capture is file I/O gated by log level - zero overhead at INFO (production default)

### Testing Strategy

#### Unit tests

- Work struct serde roundtrip without `files` field
- Work struct backward compat: old JSONL with `files` present still deserializes (serde ignores unknown fields)
- Bundle struct serde roundtrip without `loose_files` field
- Bundle struct backward compat: old JSONL with `loose_files` present still deserializes
- `classify_conflict()` uses `bundle.touched_paths` (not `work.files`); detects overlap correctly
- `classify_conflict()` returns None when bundles touch different files
- `resolve_plan_id()` traverses Work -> Phase -> Spec -> Plan correctly
- `resolve_plan_id()` returns None for orphaned records (missing parent)
- Tick with `plan_id` field serde roundtrip
- `should_capture_conversations()` returns correct values for each log level
- `drain_events()` collapses multiple `bundle.merged` events to latest SHA, preserves `tick.published` in single pass
- `drain_events()` handles interleaved event types without dropping any
- Integration branch name derivation from plan_id
- `rebase_lag` resets to 0 on successful rebase, increments on failure

#### Integration tests

- Worktree creation with integration branch as base_ref
- Bundle merge into integration branch (not main)
- Failed validation rolls back integration branch to pre-merge state
- Integration branch merge to main on Plan completion
- Integration branch deletion on Plan failure
- Main is untouched during Plan execution
- Conversation log file created at DEBUG level, not created at INFO level
- Conversation log contains full request/response pairs
- `WorktreeManager::refresh()` aborts cleanly on rebase failure (worktree not left in half-rebased state)

#### FSM tests

- Work FSM is unchanged - existing tests pass unmodified
- Bundle FSM is unchanged - existing tests pass unmodified

### Rollout Plan

**Phase 1: Prompt and schema cleanup** (no behavior change, safe to ship immediately)
1. Remove Same-File Rule from `prompts/decompose/work.pmt`
2. Update Parallelism section in `prompts/decompose/work.pmt`
3. Remove `files` field from Work struct, decomposer schema, all prompts
4. Remove `loose_files` field from Bundle struct (dead data without `files`)
5. Rewrite `classify_conflict()` to use `bundle.touched_paths` instead of `work.files`
6. Update tests for Work, Bundle, and `classify_conflict()`
7. Run `otto ci` to verify

**Phase 2: LLM conversation capture** (additive, no behavior change)
1. Add `conversations/` directory creation in `setup_logging()`
2. Add `conversation_log_dir` to `AgentLlmClient`
3. Add append logic to `call_streaming_with_messages()` and `complete()`
4. Add log-level gate
5. Run E2E at DEBUG level, verify conversation files are created and readable

**Phase 3: Integration branch model** (behavior change, needs E2E validation)
1. Add `rebase-on-merge` and `max-rebase-lag` to `IntegratorConfig`
2. Add `plan_id: String` to Tick domain model
3. Add `resolve_plan_id()` helper (Bundle -> Work -> Phase -> Spec -> Plan traversal)
4. Update Integrator `run_cycle()` to partition bundles by `plan_id`
5. Create integration branch on Plan execution start (Coordinator)
6. Update worktree base_ref to use integration branch
7. Update `merge_bundle_branches()` to verify + checkout integration branch
8. Add validation rollback (`git reset --hard <pre_merge_sha>`)
9. Add integration branch merge to main on Plan completion
10. Add integration branch cleanup on Plan failure
11. E2E test: verify main is clean during execution, dirty only at completion

**Phase 4: Rebase propagation** (depends on Phase 3)
1. Add `bundle.merged` event to protocol and Integrator
2. Replace `drain_tick_published()` with unified `drain_events()` returning `DrainedEvents`
3. Add rebase logic between iterations using `latest_integration_sha`
4. Add max-rebase-lag yield behavior with `rebase_lag` reset on success
5. Add queued bundle rebase in Integrator
6. Update `WorktreeManager::refresh()` to `git rebase --abort` on failure
7. E2E test: verify same-file parallel work items integrate correctly

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Removing `files` causes deserialization failure on old JSONL | Low | High | Serde ignores unknown fields by default; verified no `deny_unknown_fields` attribute |
| Integration branch left dangling after daemon crash | Medium | Low | Reconciler detects orphaned `integration/*` branches and cleans up; daemon startup prunes |
| Rebase conflict cascade (many rapid merges, each rebase conflicts) | Medium | Medium | `max-rebase-lag` config abandons worktree after N failed rebases; `rebase-on-merge: false` disables entirely |
| Conversation log files consume disk space at DEBUG level | Medium | Low | Files only created when log level != INFO; users running DEBUG expect verbose output |
| `merge_bundle_branches` race with concurrent Plan (future) | Low (deferred) | Medium | Deferred; integration branches isolate each Plan from main and from each other |
| Implementer mid-iteration rebase corrupts worktree state | Low | High | Rebase only happens between iterations (never mid-LLM-call); on failure, abort and yield to pool |
| Daemon crash during Plan-completion merge to main | Low | High | Integration branch survives crash; on restart, reconciler detects Plan in completion state with existing integration branch and retries the merge |
| Integrator starts cycle before integration branch exists | Medium | Low | Integrator verifies branch via `git rev-parse --verify` before checkout; missing branch = reject bundles, fail Tick gracefully |
| `classify_conflict()` silently disabled by `files` removal | Was High | Was Critical | **FIXED in revision:** rewritten to use `bundle.touched_paths` (actual git diff) instead of `work.files` (LLM guesses) |
| Integrator merges bundles from wrong Plan onto wrong branch | Was High | Was Critical | **FIXED in revision:** `plan_id` added to Tick; Integrator partitions bundles by `plan_id` via parent chain traversal |
| `drain_tick_published` + `drain_bundle_merged` drop events | Was High | Was High | **FIXED in revision:** unified `drain_events()` method is mandatory, not recommended |
| Half-rebased worktree blocks implementer | Medium | High | `WorktreeManager::refresh()` must `git rebase --abort` on failure before returning error |
| `rebase_lag` never resets after successful rebase | Low | Medium | Explicitly reset `rebase_lag = 0` on successful rebase; counter tracks consecutive failures, not total |

## Open Questions

- [ ] Should the integration branch merge to main use `--squash` (single commit) or `--no-ff` (preserve history)? `--no-ff` preserves the full commit graph for debugging; `--squash` gives a clean main history. Recommend `--no-ff` initially with a config option later.
- [ ] When the Implementer yields after rebase failure, should the next Implementer start from scratch or receive the partial implementation context? Current design carries `unsatisfied_ac` but not the partial code. The code is on the (possibly conflicted) branch - the next implementer gets a fresh worktree from the integration branch HEAD which already contains all merged work.
- [ ] Should conversation log rotation be added (e.g., max file size)? Single work items rarely exceed a few MB. Defer unless E2E testing shows otherwise.

## References

- `docs/design/2026-04-08-e2e-parallelism-and-recovery.md` - introduced Same-File Rule, Parallelism section, `files` field
- `docs/design/2026-04-09-reactive-execution-model.md` - dependency-driven execution model this builds on
- `docs/design/2026-02-25-orchestration-spine.md` - original Work/Bundle/Tick FSMs
- `docs/design/2026-02-26-multi-level-rwl.md` - Coordinator, Integrator, Implementer agent roles
- `src/agents/integrator.rs` - current `merge_bundle_branches()` implementation
- `src/worktree/manager.rs` - worktree lifecycle management
- `src/agents/implementer.rs` - existing `drain_tick_published()` pattern for event handling
- `src/agents/llm_client.rs` - LLM client methods that will get conversation capture
