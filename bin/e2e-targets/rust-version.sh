#!/usr/bin/env bash
# E2E target: Rust --version flag (the original E2E test)

TARGET_TIMEOUT=600

scaffold() {
    cargo init "${TARGET}" --name e2e-target
    (
        cd "${TARGET}"
        git add -A
        git commit -q -m "init"
    )
    ok "Target ready: $(wc -l < "${TARGET}/src/main.rs") lines in src/main.rs"
}

target_validation_commands() {
    cat <<'CMDS'
    - "cargo test"
CMDS
}

target_goal() {
    echo "Add a --version flag to this CLI that prints the crate version from CARGO_PKG_VERSION to stdout."
}

target_plan() {
    echo "${LOOPR_ROOT}/bin/e2e-targets/rust-version.md"
}

collect_results() {
    log "Target src/main.rs:"
    cat "${TARGET}/src/main.rs"
}

verify() {
    # Check if the binary was modified
    if (cd "${TARGET}" && git diff --quiet HEAD -- src/main.rs 2>/dev/null) && \
       (cd "${TARGET}" && git diff --quiet --cached HEAD -- src/main.rs 2>/dev/null); then
        AGENT_BRANCHES="$(cd "${TARGET}" && git branch | grep 'agent/' || true)"
        if [[ -n "${AGENT_BRANCHES}" ]]; then
            log "Agent branches found:"
            echo "${AGENT_BRANCHES}"
        else
            warn "No agent branches found"
        fi
    else
        ok "src/main.rs was modified on main"
    fi

    # Try building and running --version
    echo ""
    if (cd "${TARGET}" && cargo build --quiet 2>/dev/null); then
        VERSION_OUTPUT="$(cd "${TARGET}" && cargo run --quiet -- --version 2>&1 || true)"
        if [[ -n "${VERSION_OUTPUT}" ]]; then
            ok "--version output: ${VERSION_OUTPUT}"
        else
            warn "--version produced no output"
        fi
    else
        warn "cargo build failed on target"
    fi

    # Run tests
    echo ""
    if (cd "${TARGET}" && cargo test 2>&1 | /usr/bin/tail -5); then
        ok "cargo test completed"
    else
        warn "cargo test had failures"
    fi
}
