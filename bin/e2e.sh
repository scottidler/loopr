#!/usr/bin/env bash
set -euo pipefail

# e2e.sh - First autonomous E2E run per docs/design/2026-03-30-first-end-to-end-run.md
#
# Builds loopr, scaffolds a disposable target in /tmp, runs the pipeline
# headless, and reports the result.

LOOPR_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="/tmp/loopr-e2e-target"
TIMEOUT="${TIMEOUT:-600}"
DAEMON_PID=""

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

log()  { echo -e "${CYAN}[e2e]${NC} $*"; }
ok()   { echo -e "${GREEN}[e2e]${NC} $*"; }
warn() { echo -e "${YELLOW}[e2e]${NC} $*"; }
err()  { echo -e "${RED}[e2e]${NC} $*" >&2; }

cleanup() {
    if [[ -n "${DAEMON_PID}" ]] && kill -0 "${DAEMON_PID}" 2>/dev/null; then
        log "Shutting down daemon (PID ${DAEMON_PID})..."
        kill "${DAEMON_PID}" 2>/dev/null || true
        wait "${DAEMON_PID}" 2>/dev/null || true
    fi
}
trap cleanup EXIT

###############################################################################
# Pre-flight
###############################################################################

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    err "ANTHROPIC_API_KEY is not set"
    exit 1
fi

###############################################################################
# Step 1: Build and install
###############################################################################

log "Building loopr (release)..."
(cd "${LOOPR_ROOT}" && cargo build --release 2>&1 | /usr/bin/tail -3)
LOOPR="${LOOPR_ROOT}/target/release/loopr"

if [[ ! -x "${LOOPR}" ]]; then
    err "Build failed - binary not found at ${LOOPR}"
    exit 1
fi
ok "Binary: ${LOOPR}"

###############################################################################
# Step 2: Scaffold target
###############################################################################

if [[ -d "${TARGET}" ]]; then
    log "Cleaning previous target..."
    # Clean worktrees first to avoid git lock issues
    (cd "${TARGET}" && git worktree prune 2>/dev/null || true)
    rkvr rmrf "${TARGET}"
fi

log "Scaffolding target at ${TARGET}..."
cargo init "${TARGET}" --name e2e-target
(
    cd "${TARGET}"
    git add -A
    git commit -q -m "init"
)
ok "Target ready: $(wc -l < "${TARGET}/src/main.rs") lines in src/main.rs"

###############################################################################
# Step 3: Write config for the target
###############################################################################

CONFIG="${TARGET}/loopr.yml"
cat > "${CONFIG}" <<YAML
log_level: debug

project:
  repo_path: "${TARGET}"
  worktree_dir: .worktrees

agents:
  enabled: true
  auto_start_implementer: true
  auto_start_reviewer: true
  auto_start_coordinator: true
  pull_based_workers: true
  coordinator:
    interview_mode: skip

integrator:
  enabled: true
  interval_secs: 15
  validation_commands:
    - "cargo test"

validator:
  enabled: false
YAML
ok "Config written to ${CONFIG}"

###############################################################################
# Step 4: Kill any existing daemon
###############################################################################

PID_FILE="${HOME}/.local/share/loopr/daemon.pid"
if [[ -f "${PID_FILE}" ]]; then
    OLD_PID="$(cat "${PID_FILE}" 2>/dev/null || true)"
    if [[ -n "${OLD_PID}" ]] && kill -0 "${OLD_PID}" 2>/dev/null; then
        log "Killing existing daemon (PID ${OLD_PID})..."
        kill "${OLD_PID}" 2>/dev/null || true
        sleep 2
    fi
fi

###############################################################################
# Step 4: Start daemon from target directory
###############################################################################

DAEMON_LOG="${TARGET}/daemon.log"
log "Starting daemon from ${TARGET} (log: ${DAEMON_LOG})..."
(cd "${TARGET}" && "${LOOPR}" --config "${CONFIG}" daemon) > "${DAEMON_LOG}" 2>&1 &
DAEMON_PID=$!

# Wait for socket
SOCKET="${HOME}/.local/share/loopr/daemon.sock"
for i in $(seq 1 30); do
    if [[ -S "${SOCKET}" ]]; then
        ok "Daemon ready (PID ${DAEMON_PID})"
        break
    fi
    sleep 0.5
done

if [[ ! -S "${SOCKET}" ]]; then
    err "Daemon socket never appeared"
    exit 1
fi

###############################################################################
# Step 5: Run the E2E task
###############################################################################

GOAL="Add a --version flag to this CLI that prints the crate version from CARGO_PKG_VERSION to stdout."

PLAN="Phase 1: Add --version flag

Work 1: Add --version argument handling
- In src/main.rs, add argument parsing that checks for --version
- When --version is passed, print the version from env!(\"CARGO_PKG_VERSION\") and exit
- When no --version flag, keep the existing Hello World behavior
- Use std::env::args() for parsing (no external dependencies needed)

Work 2: Add test for --version
- Add a test that verifies the binary outputs the version when run with --version
- The version should match the version in Cargo.toml (currently 0.1.0)"

log "Starting E2E run (timeout: ${TIMEOUT}s)..."
log "Goal: ${GOAL}"
echo ""

EXIT_CODE=0
(cd "${TARGET}" && "${LOOPR}" --config "${CONFIG}" run \
    --plan "${PLAN}" \
    --timeout "${TIMEOUT}" \
    "${GOAL}") || EXIT_CODE=$?

echo ""

###############################################################################
# Step 6: Collect results
###############################################################################

echo -e "${BOLD}${CYAN}=== E2E Results ===${NC}"

case ${EXIT_CODE} in
    0)  ok "Exit code: 0 (GoalComplete)" ;;
    1)  warn "Exit code: 1 (Timeout)" ;;
    2)  warn "Exit code: 2 (NeedHelp)" ;;
    *)  err "Exit code: ${EXIT_CODE} (unexpected)" ;;
esac

echo ""
log "TaskStore state:"
(cd "${TARGET}" && "${LOOPR}" --config "${CONFIG}" diagnose state 2>/dev/null) || warn "diagnose state failed"

echo ""
log "Agent sessions:"
(cd "${TARGET}" && "${LOOPR}" --config "${CONFIG}" agent list 2>/dev/null) || warn "agent list failed"

echo ""
log "Work items:"
(cd "${TARGET}" && "${LOOPR}" --config "${CONFIG}" work list 2>/dev/null) || warn "work list failed"

echo ""
log "Bundles:"
(cd "${TARGET}" && "${LOOPR}" --config "${CONFIG}" bundle list 2>/dev/null) || warn "bundle list failed"

echo ""
log "Git log (target repo):"
(cd "${TARGET}" && git log --oneline --all 2>/dev/null) || warn "git log failed"

echo ""
log "Target src/main.rs:"
cat "${TARGET}/src/main.rs"

echo ""
echo -e "${BOLD}${CYAN}=== Verification ===${NC}"

# Check if the binary was modified
if (cd "${TARGET}" && git diff --quiet HEAD -- src/main.rs 2>/dev/null) && \
   (cd "${TARGET}" && git diff --quiet --cached HEAD -- src/main.rs 2>/dev/null); then
    # Check agent branches for changes
    AGENT_BRANCHES="$(cd "${TARGET}" && git branch | grep 'agent/' || true)"
    if [[ -n "${AGENT_BRANCHES}" ]]; then
        log "Agent branches found:"
        echo "${AGENT_BRANCHES}"
        # Check the first agent branch for changes
        FIRST_BRANCH="$(echo "${AGENT_BRANCHES}" | head -1 | tr -d '* ')"
        DIFF="$(cd "${TARGET}" && git diff main..."${FIRST_BRANCH}" -- src/main.rs 2>/dev/null || true)"
        if [[ -n "${DIFF}" ]]; then
            ok "Code changes found on ${FIRST_BRANCH}:"
            echo "${DIFF}"
        else
            warn "No changes to src/main.rs on agent branches"
        fi
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

# Dump daemon log tail on failure
if [[ ${EXIT_CODE} -ne 0 && -f "${DAEMON_LOG}" ]]; then
    echo ""
    log "Daemon log (last 50 lines):"
    /usr/bin/tail -50 "${DAEMON_LOG}"
fi

echo ""
echo -e "${BOLD}${CYAN}=== Summary ===${NC}"
echo -e "  Target:     ${TARGET}"
echo -e "  Exit code:  ${EXIT_CODE}"
echo -e "  Daemon log: ${DAEMON_LOG}"
echo -e "  Session:    ~/.local/share/loopr/sessions/latest/"
echo -e "  Agent logs: ~/.local/share/loopr/logs/agents/"
echo -e "${CYAN}==================${NC}"

exit ${EXIT_CODE}
