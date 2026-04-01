# Design Document: Noop Dirty Worktree Guard

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Implementer agents write files via `write_file` but then propose noop bundles instead of committing, causing an infinite reject loop. The executor's auto-commit is gated behind `!is_noop`, so the code is never committed. When the worktree is cleaned up, the files are lost. The reviewer correctly rejects the empty bundle, the coordinator resets the work to Ready, and the cycle repeats indefinitely. This document adds a defense-in-depth fix: a system-level guard in the executor that rejects noop bundles with dirty worktrees, plus prompt hardening to reduce how often the guard fires.

## Problem Statement

### Background

The noop bundle pathway (`docs/design/2026-04-01-noop-bundle-pathway.md`) was designed for a legitimate case: Phase 1 over-delivers, Phase 2 finds the work already done. The implementer signals "nothing to change" via `noop_reason`, the executor skips auto-commit, and the reviewer verifies the codebase state.

The worktree commit persistence fix (`docs/design/2026-04-01-worktree-commit-persistence.md`) ensures branches survive retry by removing the unconditional `git branch -D` in `WorktreeManager::create()`.

### Problem

There is a gap between these two designs. The noop pathway assumes the LLM will correctly distinguish "code already committed by a previous session" from "I just wrote it but didn't commit." In practice, the LLM conflates worktree state (files on disk) with git state (committed changes).

Observed in the lua-todo E2E run (2026-04-01):

1. Implementer writes `todo.lua` (6.9KB) via `write_file` action in the worktree
2. Implementer runs `test` tool - exits 0 (vacuous pass or file loads successfully)
3. Implementer concludes acceptance criteria are "already satisfied by current state"
4. Implementer proposes a noop bundle with `noop_reason: "todo.lua was already written and tests passed in previous iterations"`
5. Executor sees `is_noop = true`, skips auto-commit (executor.rs:793)
6. Bundle created with `head_commit: null`, `touched_paths: []`, `branch_name: ""`
7. Worktree cleaned up - uncommitted `todo.lua` is destroyed
8. Reviewer correctly rejects: "no file contents provided for review"
9. Coordinator resets work to Ready
10. New implementer picks up work, writes `todo.lua` again, same cycle repeats

The run produced 9+ implementer sessions, 7 rejected bundles, zero commits beyond `init`, and timed out after 900 seconds. The code quality was actually fine - every implementer wrote correct Lua code. They just never committed it.

### Root Cause

The `No-Op Detection` section in `implementer.pmt` says:

> If after reading the code you determine ALL acceptance criteria are already satisfied by the current state...

The LLM interprets "current state" as the worktree filesystem, not the git tree. After using `write_file`, the file exists on disk, tests pass, so the LLM takes the noop path. The prompt then says:

> Do NOT use `commit` before a noop `propose_bundle` - there is nothing to commit

This instruction, intended to prevent unnecessary commits when code was committed by a prior agent, instead prevents the implementer from committing its own work.

### Goals

- Prevent noop bundles with uncommitted changes from entering the taskstore
- Contain the failure to a single implementer session (no reviewer/coordinator cycles wasted)
- Reduce how often the LLM incorrectly takes the noop path via prompt hardening
- Zero additional IPC methods, FSM states, or agent types

### Non-Goals

- Silently converting noop bundles to normal bundles (masks the LLM's error, risks committing unintended changes)
- Eliminating the noop pathway entirely (it serves a legitimate purpose)
- Changing the reviewer's behavior (it's correctly rejecting empty bundles)

## Proposed Solution

### Overview

Two layers, defense in depth:

**Layer 1 - System Guard (executor.rs):** Before creating a noop bundle, run `git status --porcelain` in the worktree. If the output is non-empty (uncommitted changes exist), return `ActionResult::ActionError` with a message telling the implementer to commit first. This is a hard invariant: noop bundles with dirty worktrees are structurally invalid.

**Layer 2 - Prompt Hardening (implementer.pmt):** Tighten the No-Op Detection section to explicitly state that noop only applies when the code was already committed by a previous agent session. Add a rule: "If you used `write_file` or `edit_file` at any point in this session, you must `commit` first."

### Architecture

```
Current flow (broken):
  Implementer: write_file("todo.lua")
  -> test -> exit 0
  -> propose_bundle(noop_reason: "already complete")
  -> executor: is_noop=true, skip auto-commit
  -> bundle.create: head_commit=null, paths=[]
  -> reviewer: rejects empty bundle
  -> coordinator: work -> Ready -> loop forever

Proposed flow (Layer 1 catches it):
  Implementer: write_file("todo.lua")
  -> test -> exit 0
  -> propose_bundle(noop_reason: "already complete") + done  [batched]
  -> executor: git status --porcelain -> "?? todo.lua"
  -> ActionResult::ActionError("Cannot propose noop with uncommitted changes...")
  -> done action fires -> session ends (IterationOutcome::Done)
  -> NO bundle created, NO InReview transition, work stays InProgress
  -> worker pool spawns fresh implementer session
  -> Layer 2 (prompt hardening): LLM writes files, uses commit + propose_bundle (normal)
  -> executor: auto-commit fires, bundle has head_commit + paths
  -> reviewer: sees code, approves
```

### Implementation Plan

#### Change 1: Dirty Worktree Guard (executor.rs)

**File: `src/agents/executor.rs` - `ProposeBundle` handler (~line 788)**

Insert a check between the `is_noop` determination and the auto-commit block:

```rust
AgentAction::ProposeBundle {
    description,
    claims,
    noop_reason,
} => {
    let is_noop = noop_reason.is_some();
    let wi_id = work_id.ok_or_else(|| eyre!("propose_bundle requires work_id"))?;

    // Guard: reject noop bundles with uncommitted changes.
    // The LLM may have written files via write_file but then
    // incorrectly taken the noop path. Return an error so the
    // LLM can self-correct and submit a normal bundle.
    if is_noop {
        let status_output = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree_path)
            .output()
            .await;
        if let Ok(output) = status_output {
            let status = String::from_utf8_lossy(&output.stdout);
            if !status.trim().is_empty() {
                agent_log.warn(&format!(
                    "Rejected noop bundle: worktree has uncommitted changes:\n{}",
                    status.trim()
                ));
                return Ok(ActionResult::ActionError(format!(
                    "Cannot propose a noop bundle while the worktree has uncommitted \
                     changes:\n{}\n\
                     You wrote files in this session that are not committed. \
                     Use `commit` first, then propose a normal bundle (without \
                     noop_reason). Do NOT use noop_reason if you made any changes.",
                    status.trim()
                )));
            }
        }
    }

    // For normal bundles: auto-commit any pending changes...
    // (rest of existing code unchanged)
```

**Design decisions:**

- **Reject, not convert.** Returning `ActionResult::ActionError` prevents the bundle from being created. Although the batched `done` action ends the session before the LLM sees the error (see batching caveat below), rejecting is still better than silently converting because: (a) it preserves the invariant that noop bundles have clean worktrees, (b) it avoids auto-committing code the agent didn't explicitly intend to ship, and (c) the failure is logged for debugging.

- **Check only for noop.** Normal bundles already auto-commit, so dirty worktrees are handled. The guard only fires on the noop path.

- **Use `git status --porcelain`.** Stable, machine-readable output. Non-empty means dirty. Handles untracked files, modified files, and staged changes.

- **Fail fast.** The error prevents the bundle from reaching the taskstore, saving reviewer cycles and preventing the coordinator death loop.

- **Action batching caveat.** The prompt instructs implementers to batch `commit` + `propose_bundle` + `done` in a single response. When `ActionError` is returned for `propose_bundle`, the `done` action still executes and ends the session (via `IterationOutcome::Done`). This means in-band self-correction within the same session doesn't happen - the LLM never sees the error message. However, the guard still prevents the death loop because: (a) no bundle is created in the taskstore, (b) no transition to InReview occurs, (c) the work stays InProgress and gets picked up by a fresh implementer session. The prompt hardening (Layer 2) is what actually corrects the LLM's behavior on the next session.

- **Include dirty file list.** The error message should include the `git status --porcelain` output so the LLM knows which files to commit. This costs a few extra tokens and saves the LLM a `read_file` or `run_tool` call on the next iteration.

**Implementation note:** `execute_action` takes `action: &AgentAction`, so `noop_reason` is `&Option<String>` (a reference). The guard check uses `noop_reason.is_some()` which works on references. No clone needed for the check itself.

#### Change 2: Prompt Hardening (implementer.pmt)

**File: `prompts/implementer.pmt` - No-Op Detection section (~line 36)**

Replace the current section:

```
## No-Op Detection

If after reading the code you determine ALL acceptance criteria are already satisfied by the current state:
1. Run the verification tools (test, clippy, fmt) to confirm
2. If tools pass, propose a no-op bundle (skip `commit` - there are no changes):
...
3. Do NOT use `commit` before a noop `propose_bundle` - there is nothing to commit
4. Do NOT make cosmetic changes just to produce a diff
```

With:

```
## No-Op Detection

IMPORTANT: No-op bundles are ONLY for when code was already COMMITTED by a previous
agent session. If you used `write_file` or `edit_file` at ANY point in this session,
you MUST `commit` and submit a normal bundle - never noop.

If the acceptance criteria are already satisfied by code that is ALREADY COMMITTED to
git (not just present on disk from your own writes):
1. Run the verification tools (test, clippy, fmt) to confirm
2. If tools pass, propose a no-op bundle (skip `commit` - there are no changes):
...
3. Do NOT use `commit` before a noop `propose_bundle` - there is nothing to commit
4. Do NOT make cosmetic changes just to produce a diff
5. If you used `write_file` or `edit_file` at ANY point, you MUST `commit` first - it is NOT a noop
```

**Design decisions:**

- **Lead with the warning.** The most common failure mode is writing files then going noop. Put the guard at the top, not buried in a numbered list.
- **Repetition is intentional.** Rules 1-4 are unchanged for the legitimate noop case. Rule 5 reinforces the top-level warning. LLMs respond to repetition.
- **"COMMITTED to git" vs "present on disk."** Makes the distinction explicit in the LLM's terms.

### Data Model

No changes. Uses existing `ActionResult::ActionError(String)` variant.

### API Design

No new IPC methods or RPC endpoints. The guard is internal to the executor.

## Alternatives Considered

### Alternative 1: Silent Conversion (noop -> normal bundle)

- **Description:** When the guard detects uncommitted changes, auto-commit and create a normal bundle instead of returning an error.
- **Pros:** Always produces a working bundle. Zero wasted iterations.
- **Cons:** The implementer didn't explicitly choose to commit. If the files are partial, broken, or unintended, we'd ship garbage to the reviewer. Masks the LLM's reasoning error rather than correcting it.
- **Why not chosen:** Correctness over speed. The implementer should explicitly own its commit. The error costs one iteration; shipping bad code costs many.

### Alternative 2: Prompt-Only Fix

- **Description:** Only change the implementer prompt, no system guard.
- **Pros:** Zero code changes. Simple.
- **Cons:** Brittle. LLMs don't reliably distinguish filesystem state from git state even with explicit instructions. The prompt already says "Do NOT use commit before a noop" - adding more nuance doesn't guarantee compliance across thousands of runs.
- **Why not chosen:** Necessary but not sufficient. The prompt reduces frequency; the system guard guarantees correctness.

### Alternative 3: Track write_file calls in executor state

- **Description:** Maintain a `files_written: Vec<String>` in the executor context. If non-empty when noop is proposed, reject.
- **Pros:** More precise than `git status --porcelain` - knows exactly which files the agent wrote vs. pre-existing untracked files.
- **Cons:** Adds state management across action executions. The executor currently processes actions independently. Pre-existing untracked files in the worktree would be a false negative (agent didn't write them, but they're dirty). In practice, worktrees are created from a clean branch, so `git status` catches exactly the right set.
- **Why not chosen:** `git status --porcelain` is simpler and catches the same cases. Worktrees start clean, so any dirty state is from the current session.

## Technical Considerations

### Dependencies

None. Uses existing `git status --porcelain`, `ActionResult::ActionError`, and prompt infrastructure.

### Performance

One additional `git status --porcelain` call per noop bundle proposal. Negligible cost (~5ms). Only runs on the noop path, which is a minority of proposals.

### Error Recovery

Because implementers batch `propose_bundle` + `done`, the guard error does not enable in-band self-correction within the same session. Instead, recovery happens across sessions:

1. Guard fires: `ActionResult::ActionError` returned for `propose_bundle`
2. `done` action executes next in the batch, ending the session
3. No bundle created, work remains `InProgress`
4. Worker pool spawns a fresh implementer session on the same work
5. The new session sees a clean worktree (or one with prior commits if the branch was preserved)
6. With prompt hardening (Layer 2), the new LLM instance is more likely to use `commit` + `propose_bundle` (normal) + `done`
7. The auto-commit path fires, the bundle gets a real `head_commit` and `branch_name`

The guard's primary value is **preventing invalid bundles from entering the taskstore**. Without the guard, the empty noop bundle reaches the reviewer, gets rejected, the coordinator resets the work, and the cycle burns reviewer and coordinator tokens. With the guard, the failure is contained to a single implementer session exit - no reviewer or coordinator cycles wasted.

If the LLM continues to propose noop bundles across sessions, the retry limit on the work item will eventually cause the coordinator to escalate. This is correct behavior - a persistent LLM error should surface, not loop silently.

### Testing Strategy

1. **Unit test - guard rejects dirty noop:** Create a worktree, write a file (don't commit), call `execute_action` with `ProposeBundle { noop_reason: Some(...) }`, assert `ActionResult::ActionError` is returned with the expected message.
2. **Unit test - guard allows clean noop:** Create a worktree (no changes), call `execute_action` with `ProposeBundle { noop_reason: Some(...) }`, assert bundle is created successfully.
3. **Unit test - normal bundle bypasses guard:** Create a worktree, write a file, call `execute_action` with `ProposeBundle { noop_reason: None }`, assert auto-commit fires and bundle is created.
4. **Prompt test - verify noop section updated:** Assert `implementer.pmt` contains "ALREADY COMMITTED to git" and "write_file or edit_file at ANY point".
5. **E2E - lua-todo:** Re-run `bin/e2e.sh --target lua-todo`, verify: guard fires on first noop attempt, implementer self-corrects, code reaches main, all phases complete.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM fails to self-correct after guard error | Low | Medium | The lifeguard escalates after repeated failures. The error message is explicit about what to do. The prompt hardening reduces the chance of hitting the guard in the first place. |
| Guard false positive on pre-existing untracked files | Very Low | Low | Worktrees are created from clean branches. Only `.gitignore`d files could be pre-existing, and those don't appear in `git status --porcelain` (unless they're untracked). If this becomes an issue, switch to `git status --porcelain --untracked-files=no`. |
| Extra iteration cost when guard fires | Expected | Low | One iteration to self-correct (~2-5s of LLM time). Cheaper than the current infinite loop. The prompt hardening reduces frequency over time. |
| Prompt changes cause regression in legitimate noop cases | Low | Low | The legitimate noop case (code committed by a previous agent) is unchanged. The new wording only restricts noop when the current session wrote files. |
| `git status --porcelain` fails (worktree corruption, git unavailable) | Very Low | Low | Guard is permissive on failure - allows the noop to proceed. Downstream bundle creation or review will catch any real issues. |

## Open Questions

None remaining.

## Resolved Questions

- [x] **Should the guard message include the list of dirty files from `git status`?** Yes. Including the porcelain output in the error message helps the LLM know exactly which files to commit on its self-correction iteration. Token cost is trivial (a few filenames).

## References

- `src/agents/executor.rs:783-887` - ProposeBundle handler (auto-commit gate at line 793)
- `src/agents/executor.rs:1839` - `ActionResult` enum (`ActionError` variant)
- `prompts/implementer.pmt:36-48` - No-Op Detection section
- `docs/design/2026-04-01-noop-bundle-pathway.md` - Original noop design (created the feature)
- `docs/design/2026-04-01-worktree-commit-persistence.md` - Branch preservation fix (related but different root cause)
- E2E evidence: lua-todo run 2026-04-01, session logs at `~/.local/share/loopr/sessions/latest/`
