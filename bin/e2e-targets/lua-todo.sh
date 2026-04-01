#!/usr/bin/env bash
# E2E target: Lua command-line todo app with tests

TARGET_TIMEOUT=900

scaffold() {
    mkdir -p "${TARGET}"

    # Check Lua is available
    if ! command -v lua &>/dev/null; then
        err "lua is not installed"
        exit 1
    fi

    LUA_VERSION="$(lua -v 2>&1 | head -1)"
    log "Found: ${LUA_VERSION}"

    cat > "${TARGET}/README.md" <<'README'
# Todo App

A command-line todo application in Lua.

## Requirements

- Add a todo item with a title
- List all todo items (with optional filter: all, active, done)
- Mark a todo item as done by ID
- Delete a todo item by ID
- Persist todos to a file (todos.json) using a simple JSON format
- Include tests in test_todo.lua that verify all operations
- Pure Lua only - no external dependencies or package managers
README

    (
        cd "${TARGET}"
        git init -q
        echo -e "todos.json" > .gitignore
        git add -A
        git commit -q -m "init"
    )
    ok "Lua target ready at ${TARGET}"
}

target_validation_commands() {
    # Phase-scoped validation-commands in lua-todo.yml handle this.
    true
}

target_goal() {
    echo "Build a Lua command-line todo application. The app should support: add, list, done, and delete commands. Persist todos to a JSON file using pure Lua (no external dependencies). Include tests in test_todo.lua that verify all operations. Entry point is cli.lua, core logic in todo.lua."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/lua-todo.yml"
}

collect_results() {
    for f in todo.lua cli.lua test_todo.lua; do
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
    for f in todo.lua cli.lua test_todo.lua; do
        if [[ -f "${TARGET}/${f}" ]]; then
            ok "${f} exists"
        else
            warn "${f} missing"
            pass=false
        fi
    done

    # Run the CLI
    echo ""
    if (cd "${TARGET}" && lua cli.lua add "Test item from e2e" 2>&1); then
        ok "cli.lua add succeeded"
        if (cd "${TARGET}" && lua cli.lua list 2>&1 | grep -q "Test item"); then
            ok "cli.lua list shows the added item"
        else
            warn "cli.lua list did not show the added item"
            pass=false
        fi
    else
        warn "cli.lua add failed"
        pass=false
    fi

    # Run tests
    echo ""
    if (cd "${TARGET}" && lua test_todo.lua 2>&1 | /usr/bin/tail -15); then
        ok "tests completed"
    else
        warn "tests had failures"
        pass=false
    fi

    if [[ "${pass}" == "true" ]]; then
        ok "All verification checks passed"
    else
        warn "Some verification checks failed"
    fi
}
