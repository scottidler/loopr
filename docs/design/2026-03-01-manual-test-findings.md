# Manual End-to-End Test Findings

**Date:** 2026-03-01
**Test:** Coordinator → Implementer → Reviewer pipeline, building a Python CLI todo app
**Runs:** 2 (first run cut short, second run observed to deadlock)
**Result:** Partial success — first Work item code written + reviewed + accepted, but pipeline deadlocks on Work state transition

## Test Setup

- Fresh git repo at `/tmp/loopr-todo-test/`
- Custom `loopr.yml` with Python tools (pytest, py_compile)
- Goal: "Build a Python command-line todo app" with JSON persistence, CRUD commands, and pytest tests

## What Worked

1. **Coordinator FSM** — Successfully progressed through Planning → ActivatePhase → Executing
2. **Hierarchy generation** — Created Plan → Spec → Phase → 3-4 Works with proper dependency ordering
3. **Work dependency tracking** — Only the zero-dependency Work was assigned; dependent Works stayed in `Ready`
4. **Implementer file creation** — Wrote valid Python: `todo_app/__init__.py`, `todo_app/models.py`, `test_models.py`
5. **All 6 generated pytest tests pass** — Clean, well-structured dataclass model with UUID ids, serialization, round-trip
6. **Bundle lifecycle** — Proposed → Triaged → Reviewed → Accepted (full cycle observed)
7. **Review-rejection-retry** — Reviewer rejected 2 bundles before approving 1, demonstrating the feedback loop
8. **Reviewer agent** — Single-iteration review, approved with constructive feedback
9. **Learning creation** — Review summaries captured as Learning records (18 total across runs)
10. **Lifeguard (new)** — No false positives observed; implementers hit max_iterations (20) naturally, not lifeguard threshold
11. **Sandbox (new)** — No path escape attempts observed; agents stayed within worktree
12. **string_or_vec (new)** — `"args": ""` (bare string) deserialized correctly to `[""]` instead of failing parse

## Bugs Found

### Bug 5 (High): Two Implementers assigned to same Work, sharing worktree

**Observed:** Two implementer sessions were both assigned to the same work_id and shared the same `worktree_path`. Both agents wrote files concurrently to the same directory. Reproduced on both runs.

**Impact:** Race conditions — both agents can overwrite each other's files. One agent "won" the race, but this is non-deterministic.

**Root cause:** The `AssignAgent` handler in `executor.rs` doesn't check if another implementer is already running on the same Work. The Coordinator emitted two `AssignAgent` actions in the same iteration and both succeeded.

**Fix:** `AssignAgent` handler should reject assignment if an active (Starting/Running) implementer session already exists for the target work_id.

### Bug 6 (Medium): Default tools override config tools

**Observed:** Implementer ran `cargo test` (default Rust tool) even though `loopr.yml` configured Python tools. Error: `could not find Cargo.toml`.

**Impact:** Agents waste ~7 iterations running wrong tools before giving up and just writing files.

**Root cause:** The daemon loads config from CWD, but the tool entries in `loopr.yml` may not be reaching the `ToolRunner`. Needs investigation of config → ToolRunner wiring.

**Fix:** Verify tool config loading path. Ensure `AgentConfig.tools` from the project's `loopr.yml` is what `ToolRunner` uses.

### Bug 10 (Critical): Work stuck in InProgress after Bundle accepted — deadlock

**Observed:** Work had an Accepted Bundle but remained in `InProgress`. The Coordinator tried to transition it to `Integrated` and `Done` but got:
```
transition rejected: invalid transition from InProgress to Integrated for role coordinator
transition rejected: invalid transition from InProgress to Done for role coordinator
```

Dependent Works remained in `Ready` indefinitely because their dependency was never marked `Done`.

**Impact:** Complete pipeline deadlock. No further progress possible on any dependent Work.

**Root cause:** The Work state machine requires:
- `InProgress → InReview`: only `Role::Implementer` can do this
- `InReview → Integrated → Done`: subsequent transitions in the chain

When the Implementer hits `max_iterations` and fails, `run_agent_task()` in `executor.rs` transitions the Work to `Blocked`. The Coordinator then re-assigns an implementer which transitions it back to `InProgress`. But neither the failed Implementer (it's dead) nor the Coordinator (wrong role) can move it to `InReview`.

Even though a Bundle was proposed + reviewed + accepted, the Work status is disconnected from the Bundle status. No code path transitions the Work forward based on Bundle acceptance.

**Fix options:**
1. `run_agent_task()` should transition Work to `InReview` (not `Blocked`) if a Bundle was proposed before the agent failed
2. Add a `InProgress → Done` transition for `Role::Coordinator` when a Bundle is Accepted
3. Have the `accept_bundle` handler auto-transition the Work to `InReview` → `Done`

### Bug 8 (Low): Parse failure on empty JSON array `[]`

**Observed:** Coordinator failed to parse `[]` wrapped in markdown code fence. Wasted iteration.

**Root cause:** `parse_actions` code-fence stripping may not handle the ` ```json\n[]\n``` ` format correctly.

**Fix:** Investigate `parse_actions` code-fence stripping for the empty array case.

### Bug 7 (Low): Over-decomposition for simple tasks

**Observed:** Coordinator created 3 Phases with 4 Works for a 2-file Python project, even when the goal explicitly asked for "one phase, two work items."

**Mitigation:** Prompt tuning, not a code bug.

## Generated Code Quality

The implementer produced clean, working Python:

```python
# todo_app/models.py — dataclass with UUID id, ISO timestamps, serialization
@dataclass
class Todo:
    title: str
    id: str = field(default_factory=lambda: str(uuid.uuid4()))
    completed: bool = False
    created_at: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())

    def to_dict(self) -> dict: ...
    def from_dict(cls, data: dict) -> "Todo": ...
```

Tests: 6/6 passing — creation defaults, unique IDs, serialization round-trip, from_dict defaults.

## Metrics

| Metric | Run 1 | Run 2 |
|--------|-------|-------|
| Time to Plan | ~9s | ~9s |
| Time to hierarchy complete | ~2 min | ~2 min |
| Time to first file written | ~30s | ~30s |
| Time to Bundle proposed | ~1.5 min | ~2 min |
| Time to Bundle reviewed | ~30s | ~30s |
| Bundles proposed | 1 | 3 (2 rejected, 1 accepted) |
| Coordinator iterations | 18 | 26+ (stuck) |
| Implementer max iterations hit | 0 | 2 (both failed at 20) |
| Reviewer completions | 1 | 2 |
| Tests passing | 6/6 | 6/6 |
| Pipeline completed? | Partial (shutdown) | Deadlocked (Bug 10) |

## Priority Fix Order

1. **Bug 10** — Work state deadlock (Critical). Without this, no multi-Work pipeline can complete.
2. **Bug 5** — Duplicate agent assignment (High). Causes wasted API cost and race conditions.
3. **Bug 6** — Tool config loading (Medium). Blocks non-Rust projects.
4. **Bug 8** — Empty array parse failure (Low). Wastes iterations.

## Conclusion

The orchestration spine is functional. The system successfully breaks goals into work items, assigns agents, writes code, reviews it, and tracks state through the Bundle lifecycle. Generated code quality is good (all tests pass).

However, the pipeline cannot complete end-to-end due to Bug 10 (Work state deadlock). Once the first Work's Bundle is accepted, the Work stays stuck in `InProgress` because no actor has the authority to advance it to `Done`. This blocks all dependent Work items.

The new Bug 1-4 fixes from this PR (sandbox, lifeguard, string_or_vec) worked correctly with no regressions. The sandbox prevented no attacks (none attempted), the lifeguard triggered no false positives, and string_or_vec correctly handled bare-string args from the LLM.
