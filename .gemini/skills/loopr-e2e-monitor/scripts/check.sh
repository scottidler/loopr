#!/usr/bin/env bash
# Loopr E2E health check
# Usage: ./check.sh [target-dir]
# Defaults to /tmp/loopr-e2e/lua-todo/latest

TARGET="${1:-/tmp/loopr-e2e/lua-todo/latest}"

section() { echo; echo "=== $* ==="; }
ok()      { echo "[OK]    $*"; }
warn()    { echo "[WARN]  $*"; }
alert()   { echo "[ALERT] $*"; }

section "LOOPR E2E HEALTH CHECK"
echo "Target : $TARGET"
echo "Time   : $(date)"

# ── 1. Daemon Health ─────────────────────────────────────────────────────────
section "1. Daemon Health"
DAEMON_PROCS=$(ps aux | grep '[l]oopr' | grep 'daemon')
if [ -n "$DAEMON_PROCS" ]; then
    ok "Daemon process running"
    echo "$DAEMON_PROCS"
else
    alert "No daemon process found — orchestrator may have crashed or not started"
fi

DAEMON_LOG="$TARGET/daemon.log"
echo
echo "--- daemon.log (last 20 lines) ---"
if [ -f "$DAEMON_LOG" ]; then
    /usr/bin/tail -n 20 "$DAEMON_LOG"
    if grep -qi "panic\|thread.*panicked\|SIGABRT" "$DAEMON_LOG"; then
        alert "Panic detected in daemon.log!"
    fi
else
    warn "daemon.log not found at $DAEMON_LOG"
fi

DECOMPOSER_LOG=$(ls -t "$TARGET/agents/"decomposer-*.log 2>/dev/null | head -n 1)
echo
echo "--- decomposer log (last 20 lines) ---"
if [ -n "$DECOMPOSER_LOG" ] && [ -f "$DECOMPOSER_LOG" ]; then
    echo "Found log: $DECOMPOSER_LOG"
    /usr/bin/tail -n 20 "$DECOMPOSER_LOG"
    if grep -qi "error\|panic\|failed" "$DECOMPOSER_LOG"; then
        warn "Errors detected in $DECOMPOSER_LOG!"
    fi
else
    warn "decomposer-*.log not found in $TARGET/agents/"
fi

# ── 2. Git Worktree State ─────────────────────────────────────────────────────
section "2. Git Worktree State"
if [ -d "$TARGET/.git" ]; then
    git -C "$TARGET" worktree list
    WORKTREE_DIR="$TARGET/.worktrees"
    if [ -d "$WORKTREE_DIR" ]; then
        for wt in "$WORKTREE_DIR"/*/; do
            [ -d "$wt" ] || continue
            wt_id=$(basename "$wt")
            if [ -d "$wt/.git" ] || [ -f "$wt/.git" ]; then
                dirty=$(git -C "$wt" status --porcelain)
                if [ -n "$dirty" ]; then
                    warn "Worktree $wt_id has uncommitted changes:"
                    echo "$dirty"
                else
                    ok "Worktree $wt_id is clean"
                fi
            fi
        done
    else
        ok "No .worktrees directory (no active worktrees)"
    fi
else
    warn "No git repo found at $TARGET"
fi

# ── 3. Taskstore: Works ───────────────────────────────────────────────────────
section "3. Taskstore — Works (last 10)"
WORKS="$TARGET/.taskstore/works.jsonl"
SESSIONS="$TARGET/.taskstore/agent_sessions.jsonl"
BUNDLES="$TARGET/.taskstore/bundles.jsonl"
if [ -f "$WORKS" ]; then
    /usr/bin/tail -n 10 "$WORKS" | jq -c '{id, status, assignee}'

    # Deduplicate: JSONL is append-only so each ID may appear multiple times.
    # Take the LAST record per ID to get current state (not historical InProgress states).
    IN_PROGRESS_IDS=$(jq -rn '[inputs] | group_by(.id) | .[] | last | select(.status == "InProgress") | .id' "$WORKS")
    if [ -n "$IN_PROGRESS_IDS" ]; then
        if [ -f "$SESSIONS" ]; then
            # agent_type and status are lowercase in the JSONL (e.g. "implementer", "running")
            RUNNING_WORK_IDS=$(jq -rn '[inputs] | group_by(.id) | .[] | last | select(.agent_type == "implementer" and .status == "running") | .work_id // ""' "$SESSIONS")
        else
            RUNNING_WORK_IDS=""
        fi
        # Build set of work IDs that have an active bundle (reviewer or integrator is processing them)
        if [ -f "$BUNDLES" ]; then
            BUNDLE_ACTIVE_WORK_IDS=$(jq -rn '[inputs] | group_by(.work_id) | .[] | last | select(.status != "Rejected") | .work_id' "$BUNDLES")
        else
            BUNDLE_ACTIVE_WORK_IDS=""
        fi
        while IFS= read -r wid; do
            [ -z "$wid" ] && continue
            if echo "$RUNNING_WORK_IDS" | grep -qF "$wid"; then
                ok "Work $wid InProgress — active implementer session found"
            elif echo "$BUNDLE_ACTIVE_WORK_IDS" | grep -qF "$wid"; then
                ok "Work $wid InProgress — bundle active (Proposed/Reviewed/etc), pipeline processing"
            else
                alert "Work $wid is InProgress but NO active implementer session — possible handback failure"
            fi
        done <<< "$IN_PROGRESS_IDS"
    fi
else
    warn "works.jsonl not found"
fi

# ── 4. Taskstore: Agent Sessions ─────────────────────────────────────────────
section "4. Taskstore — Agent Sessions (last 10)"
if [ -f "$SESSIONS" ]; then
    /usr/bin/tail -n 10 "$SESSIONS" | jq -c '{agent_type, status, error_message}'

    # Check error_message across ALL session records (not just latest-per-ID) for validation loops
    if jq -rn '[inputs] | .[].error_message // ""' "$SESSIONS" | grep -qi "tool validation loop\|invalid transition"; then
        alert "Rejection/validation cycle detected — system may be locked"
    fi
else
    warn "agent_sessions.jsonl not found"
fi

# ── 5. Bundles ────────────────────────────────────────────────────────────────
section "5. Taskstore — Bundles (last 5)"
if [ -f "$BUNDLES" ]; then
    /usr/bin/tail -n 5 "$BUNDLES" | jq -c '{id, work_id, status, head_commit, noop_reason}'

    NOOP_IDS=$(jq -r 'select(.noop_reason != null or .head_commit == null) | .work_id // .id' "$BUNDLES")
    if [ -n "$NOOP_IDS" ]; then
        while IFS= read -r bid; do
            [ -z "$bid" ] && continue
            WORKTREE_PATH="$TARGET/.worktrees/$bid"
            if [ -d "$WORKTREE_PATH" ]; then
                dirty=$(git -C "$WORKTREE_PATH" status --porcelain)
                if [ -n "$dirty" ]; then
                    alert "Bundle $bid is noop/null-commit BUT worktree has uncommitted files — implementer failing to commit!"
                fi
            fi
            warn "Noop or null-commit bundle: $bid"
        done <<< "$NOOP_IDS"
    fi

    REJECTIONS=$(jq -r 'select(.status == "Rejected") | "\(.id): \(.verification // "no notes")"' "$BUNDLES" | /usr/bin/tail -n 5)
    if [ -n "$REJECTIONS" ]; then
        warn "Recent reviewer rejections:"
        echo "$REJECTIONS"
    fi
else
    warn "bundles.jsonl not found"
fi

# ── 6. Learnings ─────────────────────────────────────────────────────────────
section "6. Taskstore — Learnings (last 5)"
LEARNINGS="$TARGET/.taskstore/learnings.jsonl"
if [ -f "$LEARNINGS" ]; then
    /usr/bin/tail -n 5 "$LEARNINGS" | jq -c '.'
else
    warn "learnings.jsonl not found"
fi

echo
echo "=== END HEALTH CHECK ==="
