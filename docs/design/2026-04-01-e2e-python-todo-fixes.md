# Design Document: E2E Python-Todo Three-Bug Fix

**Author:** Scott A. Idler
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The first python-todo E2E run exposed three cascading bugs: toolchain mismatch (root cause), coordinator phase misplacement (amplifier), and invalid FSM override attempts (resource waster). This document specifies the targeted fixes for each.

## Problem Statement

### Background

The python-todo E2E test scaffolds a Python project (venv, requirements.txt, README) and asks Loopr to build a todo CLI app across two phases. The run timed out at 900s despite the LLM producing correct Python code.

### Problem

Three bugs compound to prevent completion:

1. **Toolchain mismatch** - `detect_project_tools` (`detect.rs:56`) checks `MARKER_ORDER: ["package.json", "pyproject.toml", "Cargo.toml"]`. The scaffold writes `requirements.txt` but not `pyproject.toml`, so detection falls through to configured defaults (Rust tools). The implementer gets `cargo test`/`cargo fmt`, which fail on a Python project, causing the work item to be abandoned.

2. **Phase misplacement on re-plan** - The coordinator LLM creates replacement work for the abandoned task but places it in Phase 2 instead of Phase 1. Phase 2's validation (`pytest test_todo.py`) fails because `test_todo.py` is a separate work item not yet written. This creates an infinite validation loop (15+ cycles over the full 15-minute timeout).

3. **Invalid override loop** - The coordinator attempts `override_work` on Ready-state work items. The override handler (`daemon/handlers/work.rs:335`) selects `override_transitions()` when `is_override` is true. Looking at `work.rs:120-160`, override rules only have `from` states of InProgress, InReview, and Blocked - NO rules with `from: Ready`. Every attempt fails, triggering the lifeguard circuit breaker (3 identical errors = needs_help), killing the coordinator session. The supervisor restarts it, but it repeats, burning all 5 retries.

    Note: Ready->Abandoned IS valid in normal `work_transitions()` (line 91-93), so the coordinator could use `transition` instead of `override_work`. But the LLM doesn't know the difference.

### Goals

- Python-todo E2E completes successfully within 900s
- Implementer agents get language-appropriate tools for detected project types
- Coordinator does not place replacement work in the wrong phase
- Coordinator does not waste retries on invalid FSM transitions

### Non-Goals

- Adding `requirements.txt` as a detection marker (pyproject.toml is the PEP 621 standard)
- Code-level guards for invalid-source-state overrides (useful but separate concern)
- Solving the general venv-in-worktrees problem (follow-up)
- Fixing the coordinator's general replanning intelligence

## Proposed Solution

### Fix 1: Scaffold pyproject.toml for Python Detection

**File:** `bin/e2e-targets/python-todo.sh`

Add a minimal `pyproject.toml` in the `scaffold()` function, before `git init`. This triggers `detect_project_tools` -> `python_preset()`.

```toml
[project]
name = "todo-app"
requires-python = ">=3.10"

[tool.pytest.ini_options]
testpaths = ["."]
```

**Detection flow:** `pyproject.toml` is tracked by git (not in `.gitignore`), so it appears in implementer worktrees. When `detect_project_tools` runs on the worktree path (`agents/executor.rs:408-417`), it finds `pyproject.toml` and returns `python_preset()` tools: `pytest`, `ruff check .`, `ruff format --check .`.

**Venv caveat:** `.venv/` is in `.gitignore`, so it does NOT appear in git worktrees. The `python_preset()` commands use bare `pytest`/`ruff` which must be on system PATH. If they aren't, the error is "command not found: pytest" - actionable, not the baffling "could not find Cargo.toml". The implementer's work description also says "use .venv/bin/python" so the LLM can adapt. Solving venv availability in worktrees (e.g., symlinks from main repo) is a follow-up.

**Validation commands are unaffected:** The phase-scoped validation-commands in `python-todo.yml` (`.venv/bin/python -m pytest test_todo.py -v`) run via the integrator on the main branch after merge, where `.venv` exists.

### Fix 2: Coordinator Prompt - Phase Inheritance for Replacement Work

**File:** `prompts/coordinator.pmt`

Add to the Rules section:

```
- When creating replacement Work for an Abandoned task, you MUST assign the new Work to the EXACT SAME phase_id as the Abandoned task. Never move replacement work to a different Phase. Copy the phase_id from the Abandoned Work.
```

This is a prompt-level guardrail. It's appropriate because:
- The coordinator has the abandoned work item's phase_id in its context
- The fix is a simple copy, not a complex inference
- A code-level enforcement would require tracking replacement lineage (over-engineered for this failure mode)

### Fix 3: Coordinator Prompt - Override Guardrails

**File:** `prompts/coordinator.pmt`

Update the `override_work` section to clarify valid source states:

```
    override_work ONLY works on Works in these states: InProgress, InReview, Blocked.
    - NEVER use override_work on a Ready Work. Use the normal `transition` action instead.
    - NEVER override a Work to its current status (e.g., InProgress -> InProgress is invalid).
    - If a Work is Ready but its dependencies are not Done, leave it alone and focus on completing the dependency Work items first.
```

**Why this is correct:** `override_transitions()` (`work.rs:120-160`) defines rules only from InProgress, InReview, and Blocked. No rules start from Ready. The coordinator can use normal `transition` for Ready->Abandoned (in `work_transitions()` line 91-93), but `override_work` on Ready items will always fail.

## Alternatives Considered

### Alternative 1: Add requirements.txt as detection marker
- **Description:** Add "requirements.txt" to MARKER_ORDER in detect.rs
- **Pros:** Works without changing the scaffold
- **Cons:** `requirements.txt` is ambiguous, `pyproject.toml` is the PEP 621 standard
- **Why not chosen:** Adding pyproject.toml to the scaffold is more correct and benefits the project structure

### Alternative 2: Code-level guard for invalid overrides
- **Description:** Add source-state validation in the override handler before FSM check
- **Pros:** Prevents invalid attempts with a clearer error message
- **Cons:** FSM already rejects these; the real problem is the LLM retrying the same thing. Code guard without prompt fix still wastes LLM turns.
- **Why not chosen:** Prompt fix addresses the root cause (LLM behavior). Code guard is defense-in-depth for a follow-up.

### Alternative 3: Code-level phase inheritance for replacement work
- **Description:** When coordinator creates work with a title matching an abandoned item, auto-assign the same phase_id
- **Pros:** Immune to LLM prompt-following failures
- **Cons:** Requires title-matching heuristics or explicit "replaces" field
- **Why not chosen:** Over-engineered for the current failure mode

### Alternative 4: Symlink .venv into worktrees
- **Description:** Have WorktreeManager symlink the main repo's `.venv` into each created worktree
- **Pros:** All venv tools work from worktrees without PATH changes
- **Cons:** Couples worktree manager to Python concerns; symlinks may confuse some tools
- **Why not chosen:** Separate concern. Current fix (bare commands + actionable errors) is sufficient for E2E.

## Technical Considerations

### Dependencies

No new dependencies. All changes are to existing files (1 shell script, 1 prompt file).

### Testing Strategy

- **Fix 1:** `otto ci` verifies detect.rs tests still pass. No new Rust tests needed (existing `test_detect_python_project` covers pyproject.toml detection).
- **Fix 2 & 3:** Prompt changes tested by re-running the E2E: `bin/e2e.sh python-todo`.
- **Acceptance:** E2E completes with exit code 0, all verify() checks pass (todo.py, cli.py, test_todo.py exist, pytest passes, CLI works).

### Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Bare `pytest` not on PATH in worktrees | Medium | Medium | Error is actionable; work description says "use .venv/bin/python"; LLM adapts |
| LLM ignores prompt guardrails | Low | Medium | FSM rejection errors are clear; lifeguard still catches infinite loops |
| pyproject.toml confuses LLM about project structure | Low | Low | Minimal file with no build system metadata |

## Implementation Plan

1. Add minimal `pyproject.toml` to `scaffold()` in `bin/e2e-targets/python-todo.sh`
2. Add phase-inheritance rule to `prompts/coordinator.pmt` Rules section
3. Add override guardrails to `prompts/coordinator.pmt` override_work section
4. Run `otto ci` to confirm no regressions
5. Run `bin/e2e.sh python-todo` and verify completion

## Open Questions

- [ ] Should `python_preset()` resolve `.venv/bin/` paths from the main repo root? (Follow-up, not blocking)
- [ ] Should WorktreeManager symlink `.venv` into worktrees for Python projects? (Follow-up, not blocking)

## References

- `src/tools/detect.rs:56` - MARKER_ORDER and python_preset
- `src/domain/work.rs:31-116` - work_transitions() FSM rules
- `src/domain/work.rs:120-160` - override_transitions() FSM rules
- `src/daemon/handlers/work.rs:335` - override flag selects which rule set
- `src/agents/executor.rs:408-422` - per-session tool detection on worktree path
- `prompts/coordinator.pmt` - coordinator agent prompt
- `bin/e2e-targets/python-todo.sh` - E2E scaffold script
- `bin/e2e-targets/python-todo.yml` - E2E manifest with phase-scoped validation-commands
