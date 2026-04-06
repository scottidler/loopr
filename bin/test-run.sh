#!/usr/bin/env bash
set -euo pipefail

# test-run — manual integration test for loopr
#
# Spins up a fresh todo-app project in /tmp, launches loopr in debug mode,
# tails all agent logs, watches the taskstore, and optionally opens a Claude
# session to evaluate the results.
#
# A symlink /tmp/loopr-test-latest always points to the most recent run.
# The next invocation cleans up the previous run before starting.

###############################################################################
# Config
###############################################################################

LOOPR_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOOPR_BIN="${LOOPR_ROOT}/target/release/loopr"
LOGS_DIR="${HOME}/.local/share/loopr/logs"
AGENT_LOGS_DIR="${LOGS_DIR}/agents"
LATEST_LINK="/tmp/loopr-test-latest"
TIMESTAMP="$(date +%Y%m%d-%H%M%S)"
RUN_DIR="/tmp/loopr-test-${TIMESTAMP}"
CONFIG_FILE="${RUN_DIR}/loopr.yml"
MONITOR_DIR="${RUN_DIR}/.monitor"

# Language for the todo app (override with LANG=python, etc.)
TODO_LANG="${TODO_LANG:-rust}"

# How long to let loopr run before evaluating (seconds)
TIMEOUT="${TIMEOUT:-300}"

# Skip the Claude evaluation step
SKIP_EVAL="${SKIP_EVAL:-false}"

###############################################################################
# Colors
###############################################################################

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
DIM='\033[2m'
BOLD='\033[1m'
NC='\033[0m'

log()  { echo -e "${CYAN}[test-run]${NC} $*"; }
warn() { echo -e "${YELLOW}[test-run]${NC} $*"; }
err()  { echo -e "${RED}[test-run]${NC} $*" >&2; }
ok()   { echo -e "${GREEN}[test-run]${NC} $*"; }

###############################################################################
# Cleanup helpers
###############################################################################

BG_TAIL_PID=""
BG_MONITOR_PID=""
BG_AGENT_TAILS_PID=""
DAEMON_PID=""

cleanup() {
    log "Cleaning up..."

    # Kill background processes
    for pid in "${BG_TAIL_PID}" "${BG_MONITOR_PID}" "${BG_AGENT_TAILS_PID}"; do
        [[ -n "${pid}" ]] && kill "${pid}" 2>/dev/null || true
    done
    # Kill any agent tail children
    if [[ -f "${RUN_DIR}/.agent_tail_pids" ]]; then
        while read -r pid; do
            kill "${pid}" 2>/dev/null || true
        done < "${RUN_DIR}/.agent_tail_pids"
    fi

    # Shut down the daemon gracefully
    if [[ -f "${RUN_DIR}/.daemon_started" ]]; then
        log "Shutting down loopr daemon..."
        "${LOOPR_BIN}" --config "${CONFIG_FILE}" shutdown 2>/dev/null || true
        sleep 1
        # If still alive, force kill
        [[ -n "${DAEMON_PID}" ]] && kill "${DAEMON_PID}" 2>/dev/null || true
    fi

    wait 2>/dev/null || true
    log "Done. Run directory: ${RUN_DIR}"
    log "Symlink: ${LATEST_LINK} -> ${RUN_DIR}"
}

trap cleanup EXIT

###############################################################################
# Pre-flight checks
###############################################################################

if [[ ! -x "${LOOPR_BIN}" ]]; then
    err "loopr binary not found at ${LOOPR_BIN}"
    err "Run: cargo build --release"
    exit 1
fi

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
    err "ANTHROPIC_API_KEY is not set"
    exit 1
fi

###############################################################################
# Clean up previous run
###############################################################################

if [[ -L "${LATEST_LINK}" ]]; then
    PREV_DIR="$(readlink -f "${LATEST_LINK}")"
    if [[ -d "${PREV_DIR}" && "${PREV_DIR}" == /tmp/loopr-test-* ]]; then
        log "Cleaning up previous run: ${PREV_DIR}"

        # Kill any leftover daemon from previous run
        PREV_PID_FILE="${HOME}/.local/share/loopr/daemon.pid"
        if [[ -f "${PREV_PID_FILE}" ]]; then
            PREV_PID="$(cat "${PREV_PID_FILE}" 2>/dev/null || true)"
            if [[ -n "${PREV_PID}" ]] && kill -0 "${PREV_PID}" 2>/dev/null; then
                log "Killing leftover daemon (PID ${PREV_PID})..."
                kill "${PREV_PID}" 2>/dev/null || true
                sleep 1
            fi
        fi

        rm -rf "${PREV_DIR}"
    fi
    rm -f "${LATEST_LINK}"
fi

###############################################################################
# Set up project directory
###############################################################################

log "Creating run directory: ${RUN_DIR}"
mkdir -p "${RUN_DIR}" "${MONITOR_DIR}"

# Initialize a git repo (loopr requires one)
(
    cd "${RUN_DIR}"
    git init -q
    git checkout -q -b main

    # Seed the project with a README so there's an initial commit
    cat > README.md <<'SEED'
# Todo App

A command-line todo application.

## Requirements

- Add a todo item with a title
- List all todo items
- Mark a todo item as done
- Delete a todo item
- Persist todos to disk (JSON file)
- Filter todos by status (all, active, done)
SEED
    git add README.md
    git commit -q -m "initial: seed todo app project"
)

ln -sfn "${RUN_DIR}" "${LATEST_LINK}"
ok "Symlinked ${LATEST_LINK} -> ${RUN_DIR}"

###############################################################################
# Write loopr config for this run
###############################################################################

cat > "${CONFIG_FILE}" <<YAML
debug: true
log_level: debug

project:
  repo_path: "${RUN_DIR}"
  worktree_dir: .worktrees

agents:
  enabled: true
  auto_start_implementer: true
  auto_start_reviewer: true
  pull_based_workers: true
  coordinator:
    interview_mode: skip

validator:
  enabled: false

integrator:
  enabled: true
  interval_secs: 15
  validation_commands:
    - "echo 'validation placeholder'"
YAML

ok "Config written to ${CONFIG_FILE}"

###############################################################################
# Clear old agent logs for a clean tail
###############################################################################

mkdir -p "${AGENT_LOGS_DIR}"

# Truncate the main log for a clean run
: > "${LOGS_DIR}/loopr.log"

###############################################################################
# Start loopr daemon
###############################################################################

log "Starting loopr daemon in debug mode..."
"${LOOPR_BIN}" --config "${CONFIG_FILE}" --log-level debug daemon &
DAEMON_PID=$!
touch "${RUN_DIR}/.daemon_started"

# Wait for daemon socket
SOCKET="${HOME}/.local/share/loopr/daemon.sock"
for i in $(seq 1 30); do
    if [[ -S "${SOCKET}" ]]; then
        ok "Daemon ready (PID ${DAEMON_PID})"
        break
    fi
    sleep 0.5
done

if [[ ! -S "${SOCKET}" ]]; then
    err "Daemon socket never appeared at ${SOCKET}"
    exit 1
fi

###############################################################################
# Initialize taskstore
###############################################################################

log "Initializing taskstore..."
"${LOOPR_BIN}" --config "${CONFIG_FILE}" init || warn "init returned non-zero (may already be initialized)"

###############################################################################
# Set coordinator goal
###############################################################################

GOAL="Build a ${TODO_LANG} command-line todo application in ${RUN_DIR}. \
The app should support: add, list, done, delete, and filter commands. \
Persist todos to a JSON file. Include proper error handling, help text, \
and at least basic tests."

log "Setting coordinator goal..."
"${LOOPR_BIN}" --config "${CONFIG_FILE}" coordinator set-goal "${GOAL}"
ok "Goal set: ${TODO_LANG} todo app"

###############################################################################
# Print key paths for this run
###############################################################################

echo ""
echo -e "${BOLD}${CYAN}=== Loopr Trial Paths ===${NC}"
echo -e "  ${BOLD}Run directory:${NC}   ${RUN_DIR}"
echo -e "  ${BOLD}Symlink:${NC}         ${LATEST_LINK}"
echo -e "  ${BOLD}Config:${NC}          ${CONFIG_FILE}"
echo -e "  ${BOLD}TaskStore:${NC}       ${RUN_DIR}/.taskstore/"
echo -e "  ${BOLD}Daemon log:${NC}      ${LOGS_DIR}/loopr.log"
echo -e "  ${BOLD}Agent logs:${NC}      ${AGENT_LOGS_DIR}/"
echo -e "  ${BOLD}Results:${NC}         ${MONITOR_DIR}/results.md"
echo -e "${CYAN}=========================${NC}"
echo ""

###############################################################################
# Background monitors (single tail + periodic snapshots — no per-file tails)
###############################################################################

# Agent types worth tailing individually (singletons / key agents)
TAIL_AGENT_TYPES=("coordinator" "integrator")

# Record the daemon log byte offset so we only show lines from this run
DAEMON_LOG_START="$(wc -c < "${LOGS_DIR}/loopr.log" 2>/dev/null || echo 0)"

# 1) Single tail on the main daemon log
log "Tailing daemon log (from byte ${DAEMON_LOG_START})..."
tail -c +"$((DAEMON_LOG_START + 1))" -f "${LOGS_DIR}/loopr.log" &
BG_TAIL_PID=$!

# 2) Tail the most recent log for each key agent type (wait for them to appear)
BG_AGENT_TAIL_PIDS=()
(
    # Give agents a moment to start and create log files
    sleep 5
    for agent_type in "${TAIL_AGENT_TYPES[@]}"; do
        latest="$(ls -1t "${AGENT_LOGS_DIR}/agent-${agent_type}-"*.log 2>/dev/null | head -1)"
        if [[ -n "${latest}" ]]; then
            name="$(basename "${latest}" .log)"
            echo -e "${GREEN}[test-run]${NC} Tailing ${name}"
            tail -f "${latest}" | sed "s/^/[${agent_type}] /" &
            echo $! >> "${RUN_DIR}/.agent_tail_pids"
        else
            echo -e "${YELLOW}[test-run]${NC} No ${agent_type} log found yet"
        fi
    done
    wait
) &
BG_AGENT_TAILS_PID=$!

# 2) Periodic taskstore + agent log snapshot (one process, no tails)
(
    TS_DIR="${RUN_DIR}/.taskstore"
    LAST_AGENT_SNAPSHOT=""
    while true; do
        sleep 30

        # --- TaskStore snapshot ---
        if [[ -d "${TS_DIR}" ]]; then
            echo ""
            echo -e "${BOLD}${CYAN}=== TaskStore Snapshot ($(date +%H:%M:%S)) ===${NC}"
            for jsonl in "${TS_DIR}"/*.jsonl; do
                [[ -f "${jsonl}" ]] || continue
                name="$(basename "${jsonl}" .jsonl)"
                count="$(wc -l < "${jsonl}")"
                if [[ "${count}" -gt 0 ]]; then
                    echo -e "  ${BOLD}${name}${NC}: ${count} records"
                    tail -1 "${jsonl}" | python3 -c "
import sys, json
try:
    rec = json.loads(sys.stdin.read())
    parts = []
    if 'title' in rec: parts.append(rec['title'][:60])
    if 'status' in rec: parts.append(f\"status={rec['status']}\")
    if parts: print(f'    latest: {chr(34).join([\"\"])}' + ' | '.join(parts))
except: pass
" 2>/dev/null || true
                fi
            done
            echo -e "${CYAN}=============================================${NC}"
        fi

        # --- Agent log summary (just counts + most recent activity) ---
        CURRENT_AGENTS="$(ls -1t "${AGENT_LOGS_DIR}"/agent-*.log 2>/dev/null | head -5)"
        if [[ -n "${CURRENT_AGENTS}" && "${CURRENT_AGENTS}" != "${LAST_AGENT_SNAPSHOT}" ]]; then
            LAST_AGENT_SNAPSHOT="${CURRENT_AGENTS}"
            TOTAL_AGENT_LOGS="$(ls -1 "${AGENT_LOGS_DIR}"/agent-*.log 2>/dev/null | wc -l)"
            echo -e "${BOLD}${CYAN}=== Agent Logs (${TOTAL_AGENT_LOGS} total, 5 most recent) ===${NC}"
            echo "${CURRENT_AGENTS}" | while read -r logfile; do
                [[ -f "${logfile}" ]] || continue
                name="$(basename "${logfile}" .log)"
                lines="$(wc -l < "${logfile}")"
                last_line="$(tail -1 "${logfile}" | cut -c1-120)"
                echo -e "  ${DIM}${name}${NC} (${lines}L): ${last_line}"
            done
            echo -e "${CYAN}=============================================${NC}"
        fi
        echo ""
    done
) &
BG_MONITOR_PID=$!

###############################################################################
# Wait for loopr to work
###############################################################################

log "Loopr is running. Timeout: ${TIMEOUT}s"
log "Press Ctrl+C to stop early and evaluate."
echo ""

# Wait, but allow Ctrl+C to break out early
SECONDS=0
while (( SECONDS < TIMEOUT )); do
    # Check daemon is still alive
    if ! kill -0 "${DAEMON_PID}" 2>/dev/null; then
        warn "Daemon exited early after ${SECONDS}s"
        break
    fi
    sleep 5
done

echo ""
log "Timeout reached (${SECONDS}s). Collecting results..."

###############################################################################
# Snapshot results
###############################################################################

RESULTS_FILE="${MONITOR_DIR}/results.md"

{
    echo "# Loopr Test Run Results"
    echo ""
    echo "- **Timestamp:** ${TIMESTAMP}"
    echo "- **Language:** ${TODO_LANG}"
    echo "- **Run dir:** ${RUN_DIR}"
    echo "- **Duration:** ${SECONDS}s"
    echo ""

    echo "## TaskStore State"
    echo ""
    TS_DIR="${RUN_DIR}/.taskstore"
    if [[ -d "${TS_DIR}" ]]; then
        for jsonl in "${TS_DIR}"/*.jsonl; do
            [[ -f "${jsonl}" ]] || continue
            name="$(basename "${jsonl}" .jsonl)"
            count="$(wc -l < "${jsonl}")"
            echo "### ${name} (${count} records)"
            echo ""
            if [[ "${count}" -gt 0 ]]; then
                echo '```json'
                cat "${jsonl}"
                echo '```'
            fi
            echo ""
        done
    else
        echo "*No taskstore directory found.*"
    fi

    echo "## Project Files"
    echo ""
    echo '```'
    (cd "${RUN_DIR}" && find . -not -path './.git/*' -not -path './.taskstore/*' -not -path './.worktrees/*' -not -path './.monitor/*' -type f | sort)
    echo '```'
    echo ""

    echo "## Git Log"
    echo ""
    echo '```'
    (cd "${RUN_DIR}" && git log --oneline --all 2>/dev/null || echo "(no commits)")
    echo '```'
    echo ""

    echo "## Daemon Log (last 100 lines)"
    echo ""
    echo '```'
    tail -100 "${LOGS_DIR}/loopr.log"
    echo '```'
} > "${RESULTS_FILE}"

ok "Results snapshot written to ${RESULTS_FILE}"

###############################################################################
# Success criteria checks
###############################################################################

TS_DIR="${RUN_DIR}/.taskstore"
PASS=true

check_collection() {
    local name="$1"
    local file="${TS_DIR}/${name}.jsonl"
    if [[ -f "${file}" ]] && [[ "$(wc -l < "${file}")" -gt 0 ]]; then
        ok "  ${name}: $(wc -l < "${file}") record(s)"
    else
        err "  ${name}: MISSING or empty"
        PASS=false
    fi
}

echo ""
echo -e "${BOLD}${CYAN}=== Success Criteria ===${NC}"
check_collection "plans"
check_collection "specs"
check_collection "phases"

if [[ "${PASS}" == "true" ]]; then
    ok "All success criteria met"
else
    warn "Some criteria not met (see above)"
fi
echo ""

###############################################################################
# Claude evaluation (optional)
###############################################################################

if [[ "${SKIP_EVAL}" == "true" ]]; then
    log "Skipping Claude evaluation (SKIP_EVAL=true)"
else
    if command -v claude &>/dev/null; then
        log "Running Claude evaluation..."
        echo ""

        EVAL_PROMPT="You are evaluating a test run of the 'loopr' orchestrator. \
Loopr was given the goal of building a ${TODO_LANG} command-line todo app in ${RUN_DIR}. \
Analyze the results below and report:

1. Did loopr successfully create plans/specs/phases/work items?
2. Were implementer agents spawned and did they produce code?
3. Were reviewer agents spawned and did they review bundles?
4. Is there actual ${TODO_LANG} source code in the project directory?
5. Does the code look reasonable (proper structure, error handling, tests)?
6. What went wrong, if anything?
7. Overall verdict: SUCCESS, PARTIAL, or FAILURE with explanation.

Results file: ${RESULTS_FILE}
Project directory: ${RUN_DIR}

Examine the results file and the project directory, then give your evaluation."

        claude -p "${EVAL_PROMPT}" \
            --allowedTools "Read,Glob,Grep,Bash(read-only commands)" \
            2>&1 | tee "${MONITOR_DIR}/evaluation.md"

        ok "Evaluation saved to ${MONITOR_DIR}/evaluation.md"
    else
        warn "Claude CLI not found — skipping evaluation."
        warn "Install: https://docs.anthropic.com/en/docs/claude-code"
    fi
fi

###############################################################################
# Summary
###############################################################################

echo ""
echo -e "${BOLD}${CYAN}=== Test Run Summary ===${NC}"
echo -e "  Run directory:  ${RUN_DIR}"
echo -e "  Symlink:        ${LATEST_LINK}"
echo -e "  Config:         ${CONFIG_FILE}"
echo -e "  Results:        ${RESULTS_FILE}"
echo -e "  Daemon log:     ${LOGS_DIR}/loopr.log"
echo -e "  Agent logs:     ${AGENT_LOGS_DIR}/"
if [[ -f "${MONITOR_DIR}/evaluation.md" ]]; then
echo -e "  Evaluation:     ${MONITOR_DIR}/evaluation.md"
fi
echo -e "${CYAN}=========================${NC}"
echo ""
ok "Done. Review ${LATEST_LINK} for the latest run."
