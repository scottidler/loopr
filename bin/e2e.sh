#!/usr/bin/env bash
set -euo pipefail

# e2e.sh - Parameterized E2E test runner for loopr
#
# Usage:
#   bin/e2e.sh                        # default: rust-version
#   bin/e2e.sh --target python-todo   # Python todo app
#   bin/e2e.sh --target rust-version  # explicit Rust target

LOOPR_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="/tmp/loopr-e2e-target"
DAEMON_PID=""

# Parse args
E2E_TARGET="rust-version"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) E2E_TARGET="$2"; shift 2 ;;
        *) err "Unknown argument: $1"; exit 1 ;;
    esac
done

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
# Load target definition
###############################################################################

TARGET_FILE="${LOOPR_ROOT}/bin/e2e-targets/${E2E_TARGET}.sh"
if [[ ! -f "${TARGET_FILE}" ]]; then
    err "Unknown target: ${E2E_TARGET}"
    err "Available targets:"
    for f in "${LOOPR_ROOT}"/bin/e2e-targets/*.sh; do
        err "  $(basename "${f}" .sh)"
    done
    exit 1
fi

# shellcheck source=/dev/null
source "${TARGET_FILE}"
log "Target: ${E2E_TARGET}"

TIMEOUT="${TIMEOUT:-${TARGET_TIMEOUT:-600}}"

###############################################################################
# Pre-flight
###############################################################################

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    err "ANTHROPIC_API_KEY is not set"
    exit 1
fi

###############################################################################
# Step 1: Build loopr
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
# Step 2: Scaffold target (delegated to target definition)
###############################################################################

if [[ -d "${TARGET}" ]]; then
    log "Cleaning previous target..."
    (cd "${TARGET}" && git worktree prune 2>/dev/null || true)
    rkvr rmrf "${TARGET}"
fi

log "Scaffolding target at ${TARGET}..."
scaffold

###############################################################################
# Step 3: Write config for the target
###############################################################################

CONFIG="${TARGET}/loopr.yml"
VALIDATION_CMDS="$(target_validation_commands)"
{
cat <<YAML
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
YAML
if [[ -n "${VALIDATION_CMDS}" ]]; then
    echo "  validation_commands:"
    echo "${VALIDATION_CMDS}"
fi
cat <<YAML

validator:
  enabled: false
YAML
} > "${CONFIG}"
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
# Step 5: Start daemon from target directory
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
# Step 6: Run the E2E task
###############################################################################

GOAL="$(target_goal)"
PLAN="$(target_plan)"

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
# Step 7: Collect results
###############################################################################

echo -e "${BOLD}${CYAN}=== E2E Results (${E2E_TARGET}) ===${NC}"

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

# Target-specific result collection
echo ""
collect_results

echo ""
echo -e "${BOLD}${CYAN}=== Verification ===${NC}"
verify

# Dump daemon log tail on failure
if [[ ${EXIT_CODE} -ne 0 && -f "${DAEMON_LOG}" ]]; then
    echo ""
    log "Daemon log (last 50 lines):"
    /usr/bin/tail -50 "${DAEMON_LOG}"
fi

echo ""
echo -e "${BOLD}${CYAN}=== Summary ===${NC}"
echo -e "  Target:     ${E2E_TARGET}"
echo -e "  Directory:  ${TARGET}"
echo -e "  Exit code:  ${EXIT_CODE}"
echo -e "  Daemon log: ${DAEMON_LOG}"
echo -e "  Session:    ~/.local/share/loopr/sessions/latest/"
echo -e "  Agent logs: ~/.local/share/loopr/logs/agents/"
echo -e "${CYAN}==================${NC}"

exit ${EXIT_CODE}
