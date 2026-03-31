# Design Document: Multi-Language E2E Test Suite

**Author:** Scott Idler + Claude
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Expand the E2E test suite beyond the single-file Rust `--version` flag test to
prove loopr can orchestrate multi-file, multi-language projects. Start with a
Python todo app, then graduate to Node.js.

## Problem Statement

### Background

The current `bin/e2e.sh` proves the control loop works: plan, implement,
review, merge, goal complete. But the target is trivial - a 3-line Rust CLI
getting one function added. This doesn't stress multi-file coordination,
cross-ecosystem validation, or the coordinator's decomposition ability.

### Problem

We have no evidence that loopr can:
- Decompose a multi-file project into coherent Work items
- Write files that import from each other correctly
- Use non-Rust validation commands (pytest, npm test)
- Handle projects that need dependency installation (pip, npm)

### Goals

- Python todo app E2E that reaches GoalComplete
- Parameterized `e2e.sh` that accepts different targets
- Validation commands configurable per target ecosystem
- Prove multi-file bundle creation and review works

### Non-Goals

- Docker/containerization (tests Docker, not loopr)
- Frontend frameworks (Next.js, React) - too much boilerplate noise for now
- Database-backed persistence (SQLite etc.) - keep it file-based JSON
- CI/CD integration of E2E tests (they cost real API tokens)

## Proposed Solution

### Overview

Three changes:

1. **Parameterize `bin/e2e.sh`** - accept a `--target` flag that selects the
   goal, plan, scaffold, config, and verification steps
2. **Add Python todo target** - Flask/stdlib HTTP todo app with pytest
3. **Add Node todo target** (future) - Express todo app with jest/vitest

### Phase 1: Parameterize e2e.sh

Add `--target` flag with `rust-version` as default (current behavior).

Each target provides:
- `scaffold()` - creates the initial project in `$TARGET`
- `GOAL` - the coordinator's goal string
- `PLAN` - the pre-written plan with phases and work items
- `VALIDATION_COMMANDS` - array for `loopr.yml` integrator config
- `verify()` - post-run assertions (build, run, check output)

```bash
# Usage
bin/e2e.sh                        # default: rust-version
bin/e2e.sh --target python-todo   # Python todo app
bin/e2e.sh --target node-todo     # Node todo app (future)
```

Target definitions live in `bin/e2e-targets/` as sourced shell files:
```
bin/e2e-targets/
  rust-version.sh    # current behavior, extracted
  python-todo.sh     # new
  node-todo.sh       # future
```

### Phase 2: Python Todo Target

**Scaffold:** `cargo init` replaced with a minimal Python project in a venv:
```bash
python3 -m venv "${TARGET}/.venv"
source "${TARGET}/.venv/bin/activate"
echo "pytest" > "${TARGET}/requirements.txt"
pip install -r "${TARGET}/requirements.txt"
```
```
e2e-target/
  .venv/
  README.md
  requirements.txt   # pytest
```

**Goal:**
```
Build a Python command-line todo application. The app should support:
add, list, done, and delete commands. Persist todos to a JSON file.
Include proper error handling and tests using pytest.
```

**Plan:**

All work items are in a single phase to avoid the validation mismatch
problem: the integrator runs `pytest test_todo.py` on merge, so the test
file must exist before any bundle can be validated.

```
Phase 1: Python todo app with tests

Work 1: Create todo.py with TodoStore class
- TodoStore manages a list of todo dicts with id, title, done fields
- load() reads from todos.json, save() writes to todos.json
- add(title) creates a new todo, returns it
- list_todos(filter=None) returns all todos, optionally filtered by done status
- done(id) marks a todo as done
- delete(id) removes a todo
- Use json module for persistence, no external dependencies
- IMPORTANT: use .venv/bin/python for all python/pytest commands

Work 2: Create cli.py with argparse CLI (depends on Work 1)
- Subcommands: add, list, done, delete
- add takes a title argument
- list takes optional --done/--all/--active filter
- done and delete take an id argument
- Pretty-print output with status indicators
- from todo import TodoStore
- IMPORTANT: use .venv/bin/python for all python/pytest commands

Work 3: Create test_todo.py with pytest tests (depends on Work 1)
- Test TodoStore CRUD operations using tmp_path fixture
- Test add creates a todo with correct fields
- Test done marks the correct todo
- Test delete removes the correct todo
- Test list filtering works
- Test persistence: add, reload, verify data survived
- IMPORTANT: use .venv/bin/pytest for running tests
```

**Validation commands:**
```yaml
validation_commands:
  - ".venv/bin/python -m pytest test_todo.py -v"
```

**Verification (in e2e.sh):**
```bash
# Check files exist
test -f "${TARGET}/todo.py"
test -f "${TARGET}/cli.py"
test -f "${TARGET}/test_todo.py"

# Run the CLI using venv Python
cd "${TARGET}" && .venv/bin/python cli.py add "Test item"
cd "${TARGET}" && .venv/bin/python cli.py list | grep "Test item"

# Run tests using venv pytest
cd "${TARGET}" && .venv/bin/python -m pytest test_todo.py -v
```

### Phase 3: Node Todo Target (Future)

Deferred until Python E2E is green. Same structure: scaffold with
`npm init -y`, goal for Express todo API, jest tests, validation via
`npm test`.

## Alternatives Considered

### Alternative 1: Open-Ended Goal (No Pre-Written Plan)

- **Description:** Just give the coordinator a goal and let it plan freely.
- **Pros:** Tests the full planning capability.
- **Cons:** Non-deterministic, hard to assert on, may produce wildly different
  structures between runs. The existing `test-run.sh` already does this.
- **Why not chosen:** E2E tests need deterministic success criteria. A
  pre-written plan ensures we test execution, not planning creativity.

### Alternative 2: Start with Next.js

- **Description:** Jump straight to a complex framework target.
- **Pros:** Proves maximum capability.
- **Cons:** Massive boilerplate (node_modules, tsconfig, next-env.d.ts). Hard
  to debug LLM hallucinations in unfamiliar framework structure. Package
  install adds network dependency and latency.
- **Why not chosen:** Python is denser, more explicit, zero boilerplate. Debug
  first in a clean environment, then add complexity.

### Alternative 3: Separate Scripts Per Target

- **Description:** `bin/e2e-rust.sh`, `bin/e2e-python.sh`, etc.
- **Pros:** Simple, independent.
- **Cons:** Duplicates all the daemon startup, monitoring, and teardown logic.
- **Why not chosen:** Parameterized single script is cleaner.

## Technical Considerations

### Dependencies

- Python 3.x and pytest must be available on the test machine
- No new Rust crate dependencies

### Validation Command Agnosticism

The `validation_commands` in `loopr.yml` are already arbitrary shell commands.
No code changes needed - just configure the right command per target. The
integrator runs whatever is configured.

### Timeout Considerations

Python todo app is more work than a `--version` flag. The 600s timeout may
need bumping to 900s for multi-phase targets. The `--target` config can
override `TIMEOUT`.

### Testing Strategy

- Run `bin/e2e.sh --target python-todo` manually after implementation
- Success: exit code 0 (GoalComplete), files exist, pytest passes, CLI works
- The E2E itself IS the test - no unit tests needed for the script

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM writes Python that imports non-existent modules | Med | Med | Plan specifies "no external dependencies" for core; pytest is the only dep |
| Implementer writes all files in one Work item | Low | Med | Pre-written plan decomposes into 3 focused Work items |
| pytest not installed on test machine | Low | High | Verification step checks `python -m pytest --version` in preflight |
| Coordinator ignores the pre-written plan | Low | Med | `loopr run --plan` forces the plan; coordinator executes, doesn't re-plan |
| Python files have import errors across modules | Med | Med | Plan keeps imports simple: cli.py imports from todo.py only |

## Open Questions

- [x] ~~Should the Python target use a venv?~~ Yes. Scaffold creates `.venv/`
      and validation commands use `.venv/bin/python` to avoid host pollution.
- [ ] Should we add a `--timeout` flag to `e2e.sh` per-target, or let each
      target definition override the default?

## References

- Current E2E script: `bin/e2e.sh`
- Current E2E design: `docs/design/2026-03-30-first-end-to-end-run.md`
- Rejection recovery (fixes from this session):
  `docs/design/2026-03-31-rejection-recovery-circuit-breaker.md`
