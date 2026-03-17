# Design Document: MVP8 — Worktree Isolation, Implementer Reliability, Agent Observability

**Author:** Scott Idler
**Date:** 2026-02-28
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

MVP8 fixes the critical pipeline failures discovered during end-to-end testing (build a TODO app). Implementers fail to produce valid Bundles due to three interrelated problems: worktree creation races leave sessions without a working directory, the git branch in worktrees resolves to `main` instead of the feature branch, and implementers exhaust their iteration budget without proposing. A fourth issue — no CLI visibility into per-agent iteration logs — makes all three problems harder to diagnose. This MVP fixes the worktree lifecycle, hardens the Implementer's propose-or-fail contract, and adds `loopr agent output` for observability.

## Problem Statement

### Background

MVPs 1–4 built the orchestration spine, persistence, agent pipeline (Implementer + Reviewer), and multi-level Coordinator. The system passes 1553 unit tests and all FSM/data-flow verification checks. However, the first real end-to-end run (Coordinator generates Plan → Spec → Phase → Works → Implementers write code → Bundles proposed → Reviewer reviews → Integrator merges) exposed critical failures in the Implementer → Bundle → Tick pipeline.

### Problem

During the TODO app build test, **zero Ticks were published**. The pipeline stalled at the Implementer stage:

1. **Worktree race condition** — The Coordinator spawns 2 Implementers per Work (pool min=2). Both call `worktree.create` for the same `work_id`. The first succeeds; the second fails silently. The losing session gets `worktree_path = null` and fails immediately at iteration 0.

2. **Wrong branch in worktree** — Implementers that DO get a worktree run `git rev-parse --abbrev-ref HEAD` to determine the branch name for `ProposeBundle`. This returns `"main"` instead of the expected `"agent/<work-id>"` branch created by `WorktreeManager::create()`. Bundles arrive with `branch_name: "main"`, `claims: []`, `touched_paths: []` and get rejected by the Reviewer.

3. **Iteration exhaustion without proposal** — Implementers hit 20 iterations (max) without calling `ProposeBundle`. The prompt says "only propose after tests pass, clippy clean, fmt clean" — but in a fresh repo with no build toolchain configured, these tools fail indefinitely, and the implementer loops without an exit path.

4. **No per-agent debug output** — The `agent.output` IPC method exists (ring buffer per session) but no CLI command exposes it. Diagnosing why an implementer loops or produces empty bundles requires reading JSONL files.

### Goals

- G1: Exactly one Implementer gets a valid worktree per Work; duplicates fail fast with a clear error
- G2: `ProposeBundle` always uses the correct feature branch name, never `main`
- G3: Implementers that cannot pass validation within a budget still propose their best-effort Bundle (the Reviewer decides quality, not the Implementer)
- G4: `loopr agent output <session-id>` prints the iteration-level event log for any agent session
- G5: All changes pass `otto ci` with no regressions

### Non-Goals

- Changing the Coordinator's planning strategy or Work decomposition
- Adding new agent types
- Changing the Reviewer's acceptance criteria
- Multi-repo worktree support
- Real-time streaming of agent output to TUI (existing broadcast channel handles this; CLI is batch)

## Proposed Solution

### Overview

Four independent fixes that can be implemented and tested in any order:

| Fix | Files | Risk |
|-----|-------|------|
| F1: Worktree singleton per Work | executor.rs, manager.rs | Medium — changes spawn flow |
| F2: Branch name from worktree manager | executor.rs, manager.rs | Low — read-only query |
| F3: Best-effort propose on iteration cap | implementer.rs | Low — adds fallback path |
| F4: `agent output` CLI command | cli/mod.rs, cli/dispatch.rs | Low — new read-only command |

### F1: Worktree Singleton per Work

**Problem:** Two Implementers race on `worktree_mgr.create(work_id, base_ref)`. The first agent's call succeeds (path doesn't exist yet). The second hits the `path.exists()` check at `manager.rs:53` and returns `AlreadyExists`. The `AlreadyExists` path in `executor.rs:112-114` correctly returns the existing path. **However**, if both agents start before either finishes the filesystem check, a TOCTOU race occurs: both see `!path.exists()`, both attempt `git worktree add`, and the second `git` command fails with `GitCommand` error (branch already exists). That error falls through to the catch-all at `executor.rs:116-118` which sets `worktree_path = None`.

**Root cause in code:**

```rust
// executor.rs run_agent_task(), lines 110-120
let worktree_path = match worktree_mgr.create(key, &base_ref) {
    Ok(path) => Some(path),
    Err(WorktreeError::AlreadyExists(_)) => {
        Some(worktree_mgr.worktree_path(key))  // OK — reuses existing
    }
    Err(e) => {
        warn!("...");
        None  // <-- BUG: worktree_path stays None, implementer fails at line 274
    }
};
```

**Fix:** Replace the three-arm match with an idempotent `get_or_create` method on `WorktreeManager`:

```rust
// worktree/manager.rs — new method
pub fn get_or_create(&self, work_id: &str, base_ref: &str) -> Result<PathBuf, WorktreeError> {
    let path = self.worktree_dir.join(work_id);
    if path.exists() {
        // Verify it's a valid git worktree by checking for .git file
        let git_file = path.join(".git");
        if git_file.exists() {
            return Ok(path);
        }
        // Directory exists but isn't a worktree — clean up and recreate
        std::fs::remove_dir_all(&path)?;
    }
    // create() may fail with GitCommand if the branch "agent/<work_id>" already
    // exists (TOCTOU race with another agent). In that case, delete the stale
    // branch and retry once. Note: create() already does branch -D, but it may
    // have raced with the other agent's worktree add. If the path now exists
    // after the failed create (the other agent won), just return it.
    match self.create(work_id, base_ref) {
        Ok(p) => Ok(p),
        Err(WorktreeError::AlreadyExists(_)) => Ok(path),
        Err(e) => {
            // Check if the other racer won and the path now exists
            if path.join(".git").exists() {
                Ok(path)
            } else {
                Err(e)
            }
        }
    }
}
```

Then in `executor.rs::run_agent_task()`, replace the match on `worktree_mgr.create()`:

```rust
let worktree_path = match worktree_mgr.get_or_create(key, &base_ref) {
    Ok(path) => Some(path),
    Err(e) => {
        error!("Agent {} worktree creation failed: {}", session_id, e);
        // Fail the session immediately — don't proceed without a worktree
        fail_session(&stores, &session_id, &format!("worktree creation failed: {}", e));
        return;
    }
};
```

Key behavioral change: on failure, the session transitions to `Failed` immediately and the task returns. No more "ghost" sessions with `worktree_path = null` that only fail later inside `run_implementer()`.

### F2: Branch Name from Worktree Manager

**Problem:** `ProposeBundle` runs `git rev-parse --abbrev-ref HEAD` inside the worktree. This returns `"main"` when the worktree checkout didn't switch branches properly, or when the implementer's git operations (commit, reset) accidentally detach HEAD.

**Fix:** Don't rely on `git rev-parse` at proposal time. Instead, derive the branch name deterministically from the `work_id`, matching what `WorktreeManager::create()` uses:

```rust
// executor.rs, ProposeBundle handler (lines 532-548)
// BEFORE (lines 534-539):
let mut branch_cmd = tokio::process::Command::new("git");
branch_cmd.args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(worktree_path);
let branch_out = branch_cmd.output().await?;
let branch_name = String::from_utf8_lossy(&branch_out.stdout).trim().to_string();

// AFTER — replace those 4 lines with:
let wi_id = work_id.ok_or_else(|| eyre!("propose_bundle requires work_id"))?;
let branch_name = format!("agent/{}", wi_id);
```

This is safe because `WorktreeManager::create()` creates the branch as `format!("agent/{}", work_id)` at `manager.rs:57`. The branch name is deterministic from the work_id. The `wi_id` variable is already resolved at `executor.rs:541` — this just moves the resolution before branch name derivation.

Additionally, add a validation in `WorktreeManager::create()` that verifies the branch was actually checked out:

```rust
// After git worktree add succeeds, verify:
let verify = Command::new("git")
    .args(["branch", "--show-current"])
    .current_dir(&path)
    .output()?;
let actual_branch = String::from_utf8_lossy(&verify.stdout).trim().to_string();
if actual_branch != branch {
    return Err(WorktreeError::GitCommand(
        format!("worktree created but branch mismatch: expected '{}', got '{}'", branch, actual_branch)
    ));
}
```

### F3: Best-Effort Propose on Iteration Cap

**Problem:** The implementer prompt says "only propose_bundle after ALL of: tests pass, clippy clean, fmt clean." In a fresh repo where the build toolchain isn't set up, or where tests have legitimate failures, the implementer loops indefinitely trying to fix issues, hits 20 iterations, and fails without ever proposing.

**Fix:** Two complementary changes:

**A) Budget-exhaustion prompt injection.** When the iteration counter reaches the last 2 iterations, inject an urgent directive into the `previous_summary` context (the mechanism already exists — `previous_summary` is passed into `run_iteration` and appended to the LLM prompt):

```rust
// implementer.rs, run_implementer() loop, inside the for loop before run_iteration()
if i >= max_iterations.saturating_sub(1) {
    let budget_warning = format!(
        "\n\n## URGENT: Budget Exhausted\n\
        You have {} iteration(s) remaining. You MUST call `propose_bundle` NOW \
        with whatever code you have, even if tests fail. Commit first, then propose. \
        Include a description of what works and what doesn't. \
        The Reviewer will evaluate quality — your job is to submit.\n",
        max_iterations - i
    );
    previous_summary = Some(
        previous_summary.map_or(budget_warning.clone(), |s| format!("{}\n{}", s, budget_warning))
    );
}
```

**B) Force-propose after loop exhaustion.** Track whether a bundle was proposed with a dedicated `bool` flag. The `format_action_summary` function at `implementer.rs:394` produces `"proposed bundle: <desc>"` for `ActionResult::BundleProposed`. Since `run_iteration` joins all summaries into the `Continue(summary)` string, we detect proposals via this known prefix:

```rust
// implementer.rs — add a `has_proposed` flag before the for loop (line ~277)
let mut has_proposed = false;

// Inside the for loop, in the Continue arm (line ~331):
Ok(IterationOutcome::Continue(summary)) => {
    if summary.contains("proposed bundle:") {
        has_proposed = true;
    }
    // ... existing logic (event, log, set previous_summary)
}
// Also in the Done arm — Done after proposing means success:
Ok(IterationOutcome::Done(_)) => {
    has_proposed = true;
    // ... existing logic
}
```

After the loop exits at the cap, force-commit and propose:

```rust
// implementer.rs, after the for loop exits (before the Err return at line 351)
if !has_proposed {
    info!("Implementer {} force-proposing at iteration cap", session.id);
    // Commit whatever is in the worktree
    let _ = execute_action(
        &bridge,
        &AgentAction::Commit {
            message: format!("WIP: auto-commit at iteration cap ({})", max_iterations),
            paths: vec![".".to_string()],
        },
        &worktree_path, Some(&work_id), AgentType::Implementer,
    ).await;
    // Propose the bundle
    let _ = execute_action(
        &bridge,
        &AgentAction::ProposeBundle {
            description: format!(
                "Auto-proposed at iteration cap ({}). Tests may not pass.",
                max_iterations
            ),
            claims: vec!["partial implementation — needs review".to_string()],
        },
        &worktree_path, Some(&work_id), AgentType::Implementer,
    ).await;
}
```

This ensures the Reviewer always gets to evaluate the work, rather than silently discarding `max_iterations` of LLM output. The commit-before-propose step is necessary because the implementer may have written files without committing them.

### F4: `agent output` CLI Command

**Problem:** The `agent.output` IPC method exists at `handlers.rs:196` with a per-session ring buffer (1000 events), but no CLI command maps to it.

**Fix:** Add `Output` variant to `AgentCmd` and dispatch it:

```rust
// cli/mod.rs — add to AgentCmd enum
Output {
    session_id: String,
    #[arg(short, long, default_value = "0")]
    since: u64,
},
```

```rust
// cli/dispatch.rs — add to agent_to_ipc()
AgentCmd::Output { session_id, since } => (
    "agent.output".to_string(),
    json!({ "session_id": session_id, "since": since }),
),
```

The existing `handle_agent_output` handler returns a JSON array of `AgentEvent` objects from the ring buffer. No handler changes needed.

## Alternatives Considered

### Alternative 1: Synchronous Worktree Creation in Handler

- **Description:** Create the worktree inside `handle_agent_start` (synchronous) before spawning the async task, so the session always has `worktree_path` set before the Implementer runs.
- **Pros:** Eliminates the race entirely — session starts with a valid path or the start request fails.
- **Cons:** Git operations can be slow (100ms–1s), and `handle_agent_start` is a synchronous IPC handler. Blocking it would stall the daemon's dispatch loop for all concurrent requests.
- **Why not chosen:** The `get_or_create` approach in the async task achieves the same safety without blocking the daemon. If the worktree can't be created, the session fails immediately rather than proceeding with `null`.

### Alternative 2: Single Implementer per Work (Remove Pool)

- **Description:** Set `min_pool = 1` for implementers so only one is spawned per Work.
- **Pros:** Eliminates the race condition trivially.
- **Cons:** Reduces throughput. The pool exists so that if one implementer fails, another can retry without waiting for the Coordinator to respawn. Removing it degrades resilience.
- **Why not chosen:** The pool is a feature, not a bug. The `get_or_create` fix makes the pool safe while preserving its throughput benefits.

### Alternative 3: Remove "Only Propose After Tests Pass" Constraint

- **Description:** Change the implementer prompt to always propose after writing code, regardless of test results.
- **Pros:** Simple prompt change. No code changes.
- **Cons:** Floods the Reviewer with low-quality bundles. The constraint exists for a reason — it filters obvious garbage before consuming Reviewer LLM tokens.
- **Why not chosen:** The "best-effort propose at iteration cap" is a better compromise. It preserves the quality filter for normal operation but prevents silent iteration exhaustion.

## Technical Considerations

### Dependencies

- No new crate dependencies
- All fixes are internal to existing modules
- F1–F4 are independent of each other

### Performance

- F1 (`get_or_create`): One extra `path.exists()` + `path.join(".git").exists()` check — negligible
- F2 (deterministic branch name): Removes one `git rev-parse` subprocess call — net improvement
- F3 (last-chance propose): One extra string append to prompt on penultimate iteration — negligible
- F4 (`agent output` CLI): Read-only query on existing ring buffer — negligible

### Security

No security implications. All changes are internal agent/worktree operations. No new IPC methods with write semantics.

### Testing Strategy

**F1 — Worktree singleton:**
- Unit test: `get_or_create` returns existing path when worktree exists
- Unit test: `get_or_create` creates new worktree when directory absent
- Unit test: `get_or_create` recreates when directory exists but isn't a git worktree
- Integration test: Two concurrent `agent.start` calls for same `work_id` — both sessions get valid `worktree_path`

**F2 — Branch name:**
- Unit test: `ProposeBundle` uses `format!("agent/{}", wi_id)` not `git rev-parse`
- Unit test: `WorktreeManager::create()` verifies branch with `git branch --show-current`
- Unit test: Verification failure returns `WorktreeError`

**F3 — Best-effort propose:**
- Unit test: At `max_iterations - 2`, prompt includes "Budget Exhausted" directive
- Unit test: At `max_iterations` without prior propose, force-propose is called
- Unit test: Force-propose sets `claims` to `["partial implementation — needs review"]`
- Integration test: Implementer that never passes tests still produces a Bundle

**F4 — Agent output CLI:**
- Unit test: `AgentCmd::Output` maps to `("agent.output", {"session_id": ..., "since": ...})`
- Existing handler tests already cover `handle_agent_output`

### Rollout Plan

All four fixes ship together in a single `otto ci`-passing commit. No feature flags needed — the changes are backwards-compatible:
- F1 is a strictly safer version of the existing create path
- F2 produces the same branch name that was intended all along
- F3 only activates at the iteration cap (normal flow unchanged)
- F4 is additive (new CLI subcommand, existing handler)

## Implementation Plan

### Phase 1: Worktree Reliability (F1 + F2)

1. Add `get_or_create()` to `WorktreeManager` with idempotency logic
2. Add post-create branch verification to `WorktreeManager::create()`
3. Replace `worktree_mgr.create()` call in `run_agent_task()` with `get_or_create()`
4. On `get_or_create` failure, fail session immediately and return
5. Replace `git rev-parse --abbrev-ref HEAD` in `ProposeBundle` with `format!("agent/{}", wi_id)`
6. Tests for all new/changed behavior

### Phase 2: Implementer Reliability (F3)

1. Add `has_proposed` tracking flag to implementer loop
2. Add "Budget Exhausted" prompt injection at `max_iterations - 2`
3. Add force-propose fallback after loop exits without proposal
4. Tests for iteration-cap behavior

### Phase 3: Observability (F4)

1. Add `Output` variant to `AgentCmd` in `cli/mod.rs`
2. Add dispatch mapping in `cli/dispatch.rs`
3. Test CLI → IPC roundtrip

## End-to-End Acceptance Test

After all four fixes ship, re-run the TODO app scenario from the verification protocol (Section 6.1):

```bash
# 1. Create fresh repo
TODO_DIR=$(mktemp -d)
cd $TODO_DIR && git init && echo "# TODO" > README.md && git add . && git commit -m "init"

# 2. Start daemon with test config
loopr --config test.yml daemon

# 3. Set goal and start Coordinator
loopr coordinator set-goal "Build a TODO app with add, list, complete, delete using Rust CLI"
loopr agent start-coordinator

# 4. Monitor (repeat every 30s)
loopr work list | jq '.[] | "\(.status) \(.title[0:50])"'
loopr bundle list | jq '.[] | "\(.status) branch=\(.branch_name)"'
loopr agent output <session-id>  # NEW: debug individual agents
```

**Pass criteria:**
- At least one Bundle has `branch_name` starting with `agent/` (not `main`)
- At least one Bundle reaches `Triaged` or `Reviewed` status
- No agent sessions have `worktree_path: null` while `status: running`
- Implementers that hit iteration cap still produce a Bundle (force-proposed)
- `loopr agent output <session-id>` returns non-empty JSON array

**Stretch goal:** At least one Tick reaches `Published` status within 20 Coordinator iterations.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `get_or_create` deletes a valid worktree with uncommitted work | Low | High | Only deletes when `.git` file is missing (not a valid worktree). Valid worktrees always have `.git`. |
| Deterministic branch name mismatches worktree manager naming | Low | High | Both use `format!("agent/{}", work_id)`. Single source of truth in `WorktreeManager`. |
| Force-propose floods Reviewer with garbage bundles | Medium | Medium | Only triggers at iteration cap. Reviewer rejects bad bundles (verified working). Coordinator creates Learnings from rejections. |
| Ring buffer overflow loses early events for long-running sessions | Low | Low | Buffer is 1000 events. Implementer max_iterations is 20. Even with verbose output, unlikely to overflow. |
| Force-commit fails because no files were modified | Medium | Low | `git commit` returns non-zero, `execute_action` returns `ActionError`, force-propose proceeds anyway. An empty commit is fine — the Reviewer will reject the empty bundle. |
| Corrupt `.git` file fools `get_or_create` into returning invalid worktree | Low | Medium | The implementer's first `read_file` or `run_tool` will fail immediately, triggering a retry. Could add `git status` verification in `get_or_create`, but YAGNI for now. |

## Open Questions

- [ ] Should `get_or_create` acquire a file lock to prevent true concurrent creation? Current approach relies on the filesystem being atomic for `path.exists()`, which is safe on Linux but worth noting.
- [ ] Should force-propose include the last tool output (test failures) in the bundle description to give the Reviewer more context?
- [ ] Should `max_iterations` default be lowered from 20 to 10 now that best-effort propose exists? Lower cap means faster feedback loops.

## References

- End-to-end test results: TODO app build (2026-02-28 session)
- MVP4 design doc: `docs/design/2026-02-26-loopr-v3-mvp4.md`
- MVP3 design doc: `docs/design/2026-02-26-loopr-v3-mvp3.md` (Implementer + Reviewer agents)
- Post-implementation verification: `docs/verify-implementation.md`
