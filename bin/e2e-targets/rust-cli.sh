#!/usr/bin/env bash
# E2E target: Rust multi-subcommand notes CLI with SQLite and cargo tests

TARGET_TIMEOUT=1200

scaffold() {
    if ! command -v cargo &>/dev/null; then
        err "cargo is not installed"
        exit 1
    fi

    log "Found: $(cargo --version)"

    cargo init "${TARGET}" --name notes --quiet

    (
        cd "${TARGET}"

        # Add dependencies
        cargo add clap --features derive --quiet
        cargo add rusqlite --features bundled --quiet
        cargo add eyre --quiet

        # Add dev dependency for integration tests
        cargo add --dev tempfile --quiet

        mkdir -p tests

        cat > README.md <<'README'
# notes

A command-line notes manager backed by SQLite.

## Usage

    notes add "My Note" [--content "..."] [--tags "tag1,tag2"]
    notes list
    notes get <id>
    notes delete <id>
    notes search <query>

## Options

    --db <path>   Path to SQLite database (default: notes.db)

## Test

    cargo test
README

        git add -A
        git commit -q -m "init: cargo project with clap, rusqlite, eyre"
    )
    ok "Rust CLI target ready at ${TARGET}"
}

target_validation_commands() {
    cat <<'CMDS'
    - "cargo test"
CMDS
}

target_goal() {
    echo "Build a 'notes' CLI tool in Rust with subcommands: add, list, get, delete, search. Store notes in SQLite (rusqlite with bundled feature). Use clap (derive) for CLI. Note schema: id (INTEGER PRIMARY KEY AUTOINCREMENT), title (TEXT NOT NULL), content (TEXT default ''), tags (TEXT default ''). Database path via --db flag (default: notes.db). Include unit tests in src/db.rs and CLI integration tests in tests/cli.rs. Validate with: cargo test."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/rust-cli.yml"
}

collect_results() {
    for f in src/main.rs src/db.rs tests/cli.rs; do
        if [[ -f "${TARGET}/${f}" ]]; then
            echo ""
            log "Target ${f}:"
            cat "${TARGET}/${f}"
        fi
    done
}

verify() {
    local pass=true

    for f in src/main.rs src/db.rs tests/cli.rs; do
        if [[ -f "${TARGET}/${f}" ]]; then
            ok "${f} exists"
        else
            warn "${f} missing"
            pass=false
        fi
    done

    echo ""
    if (cd "${TARGET}" && cargo build --quiet 2>&1); then
        ok "cargo build succeeded"
    else
        warn "cargo build failed"
        pass=false
    fi

    echo ""
    if (cd "${TARGET}" && cargo test 2>&1 | /usr/bin/tail -15); then
        ok "cargo test passed"
    else
        warn "cargo test had failures"
        pass=false
    fi

    # Smoke test the binary
    echo ""
    BINARY="${TARGET}/target/debug/notes"
    DB="${TARGET}/verify_test.db"
    if [[ -x "${BINARY}" ]]; then
        ADD_OUT=$("${BINARY}" --db "${DB}" add "Verify Test" 2>&1 || true)
        if echo "${ADD_OUT}" | grep -q "Verify Test"; then
            ok "notes add works: ${ADD_OUT}"
        else
            warn "notes add did not produce expected output: ${ADD_OUT}"
            pass=false
        fi

        LIST_OUT=$("${BINARY}" --db "${DB}" list 2>&1 || true)
        if echo "${LIST_OUT}" | grep -q "Verify Test"; then
            ok "notes list shows the added note"
        else
            warn "notes list did not show expected note: ${LIST_OUT}"
            pass=false
        fi

        SEARCH_OUT=$("${BINARY}" --db "${DB}" search "Verify" 2>&1 || true)
        if echo "${SEARCH_OUT}" | grep -q "Verify Test"; then
            ok "notes search finds matching note"
        else
            warn "notes search did not find expected note: ${SEARCH_OUT}"
            pass=false
        fi

        rm -f "${DB}"
    else
        warn "Binary not found at ${BINARY}"
        pass=false
    fi

    if [[ "${pass}" == "true" ]]; then
        ok "All verification checks passed"
    else
        warn "Some verification checks failed"
    fi
}
