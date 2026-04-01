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

    cat > "${TARGET}/pyproject.toml" <<'PYPROJECT'
[project]
name = "todo-app"
requires-python = ">=3.10"

[tool.pytest.ini_options]
testpaths = ["."]
PYPROJECT

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
    # Phase-scoped validation-commands in python-todo.yml handle this now.
    # Return empty so no global validation_commands are written to loopr.yml.
    true
}

target_goal() {
    echo "Build a Python command-line todo application. The app should support: add, list, done, and delete commands. Persist todos to a JSON file. Include proper error handling and tests using pytest."
}

target_plan() {
    # Return path to YAML manifest for deterministic decomposition
    echo "${LOOPR_ROOT}/bin/e2e-targets/python-todo.yml"
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
