#!/usr/bin/env bash
# E2E target: Python command-line todo app with pytest

TARGET_TIMEOUT=900

scaffold() {
    mkdir -p "${TARGET}"

    # Check Python 3 is available
    if ! command -v python3 &>/dev/null; then
        err "python3 is not installed"
        exit 1
    fi

    # Create venv and install pytest
    log "Creating Python venv..."
    python3 -m venv "${TARGET}/.venv"
    "${TARGET}/.venv/bin/pip" install --quiet pytest

    # Seed the project
    cat > "${TARGET}/requirements.txt" <<'REQ'
pytest
REQ

    cat > "${TARGET}/README.md" <<'README'
# Todo App

A command-line todo application in Python.

## Requirements

- Add a todo item with a title
- List all todo items (with optional filter: all, active, done)
- Mark a todo item as done by ID
- Delete a todo item by ID
- Persist todos to a JSON file (todos.json)
- Include tests using pytest
README

    (
        cd "${TARGET}"
        git init -q
        # Ignore venv and json data
        echo -e ".venv/\ntodos.json\n__pycache__/\n.pytest_cache/" > .gitignore
        git add -A
        git commit -q -m "init"
    )
    ok "Python target ready at ${TARGET}"
}

target_validation_commands() {
    cat <<'CMDS'
    - ".venv/bin/python -m pytest test_todo.py -v"
CMDS
}

target_goal() {
    echo "Build a Python command-line todo application. The app should support: add, list, done, and delete commands. Persist todos to a JSON file. Include proper error handling and tests using pytest."
}

target_plan() {
    cat <<'PLAN'
Phase 1: Python todo app with tests

Work 1: Create todo.py with TodoStore class
- TodoStore manages a list of todo dicts with id, title, done fields
- load() reads from todos.json, save() writes to todos.json
- add(title) creates a new todo with a unique integer id, returns it
- list_todos(status_filter=None) returns all todos, optionally filtered by done status ("all", "active", "done")
- done(todo_id) marks a todo as done, returns True if found
- delete(todo_id) removes a todo, returns True if found
- Use json module for persistence, no external dependencies
- IMPORTANT: use .venv/bin/python for all python commands

Work 2: Create cli.py with argparse CLI (depends on Work 1)
- Subcommands: add, list, done, delete
- add takes a positional title argument
- list takes optional --filter flag (all/active/done, default: all)
- done and delete take a positional id argument (integer)
- Pretty-print output with status indicators ([x] for done, [ ] for active)
- from todo import TodoStore
- IMPORTANT: use .venv/bin/python for all python commands

Work 3: Create test_todo.py with pytest tests (depends on Work 1)
- Test TodoStore CRUD operations using tmp_path fixture for isolation
- Test add creates a todo with correct fields (id, title, done=False)
- Test done marks the correct todo as done
- Test delete removes the correct todo
- Test list_todos filtering works for all/active/done
- Test persistence: add items, create new TodoStore on same path, verify data survived
- IMPORTANT: use .venv/bin/pytest for running tests
PLAN
}

collect_results() {
    for f in todo.py cli.py test_todo.py; do
        if [[ -f "${TARGET}/${f}" ]]; then
            echo ""
            log "Target ${f}:"
            cat "${TARGET}/${f}"
        fi
    done
}

verify() {
    local pass=true

    # Check files exist
    for f in todo.py cli.py test_todo.py; do
        if [[ -f "${TARGET}/${f}" ]]; then
            ok "${f} exists"
        else
            warn "${f} missing"
            pass=false
        fi
    done

    # Run the CLI
    echo ""
    if (cd "${TARGET}" && .venv/bin/python cli.py add "Test item from e2e" 2>&1); then
        ok "cli.py add succeeded"
        if (cd "${TARGET}" && .venv/bin/python cli.py list 2>&1 | grep -q "Test item"); then
            ok "cli.py list shows the added item"
        else
            warn "cli.py list did not show the added item"
            pass=false
        fi
    else
        warn "cli.py add failed"
        pass=false
    fi

    # Run tests
    echo ""
    if (cd "${TARGET}" && .venv/bin/python -m pytest test_todo.py -v 2>&1 | /usr/bin/tail -15); then
        ok "pytest completed"
    else
        warn "pytest had failures"
        pass=false
    fi

    if [[ "${pass}" == "true" ]]; then
        ok "All verification checks passed"
    else
        warn "Some verification checks failed"
    fi
}
