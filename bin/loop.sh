#!/usr/bin/env bash
# Loopr v3 Build Loop (Ralph Wiggum Pattern)
# Fresh context each iteration — state persists in progress.txt
# Validation runs OUTSIDE the LLM session
# https://ghuntley.com/ralph/
set -e

# Configuration (override via environment variables or first positional arg)
PROMPT_FILE=${1:-${PROMPT_FILE:-PROMPT.md}}
MAX_ITERATIONS=${MAX_ITERATIONS:-100}
PROGRESS_FILE=${PROGRESS_FILE:-progress.txt}
MODEL=${MODEL:-opus}
SLEEP_BETWEEN=${SLEEP_BETWEEN:-2}
TIMEOUT_MINUTES=${TIMEOUT_MINUTES:-10}
COMPLETION_SIGNAL="<promise>COMPLETE</promise>"
VALIDATION_CMD=${VALIDATION_CMD:-"otto ci"}

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Get script directory and project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_ROOT"

# Current branch (we commit directly here, no iter branches)
CURRENT_BRANCH=$(git branch --show-current)

echo -e "${GREEN}=== Loopr v3 Build Loop (Ralph Wiggum) ===${NC}"
echo "Project:    $PROJECT_ROOT"
echo "Prompt:     $PROMPT_FILE"
echo "Progress:   $PROGRESS_FILE"
echo "Model:      $MODEL"
echo "Timeout:    ${TIMEOUT_MINUTES}m per iteration"
echo "Max:        $MAX_ITERATIONS iterations"
echo "Branch:     $CURRENT_BRANCH (committing directly)"
echo "Validation: $VALIDATION_CMD"
echo ""

# Check prompt file exists
if [[ ! -f "$PROMPT_FILE" ]]; then
    echo -e "${RED}Error: $PROMPT_FILE not found${NC}"
    exit 1
fi

# Initialize progress file if needed
if [[ ! -f "$PROGRESS_FILE" ]]; then
    echo -e "${YELLOW}Initializing progress file...${NC}"
    cat >"$PROGRESS_FILE" <<EOF
# Loopr v3 MVP1 — Ralph Progress Log
Started: $(date)
Branch: $CURRENT_BRANCH
Prompt: $PROMPT_FILE
---
EOF
fi

# Quality gate checks — returns 0 if clean, 1 if issues found
check_quality_gates() {
    local issues_found=0

    # Check 1: No #[allow(dead_code)] markers in src/
    local dead_code_markers
    dead_code_markers=$(grep -rn "allow(dead_code)" src/ 2>/dev/null || true)
    if [[ -n "$dead_code_markers" ]]; then
        echo "Quality gate FAILED: #[allow(dead_code)] markers found"
        echo "$dead_code_markers"
        local count
        count=$(echo "$dead_code_markers" | wc -l)
        echo "  Total: $count markers — all must be removed"
        issues_found=1
    fi

    # Check 2: No underscore-prefixed parameters in non-trait-impl code
    local underscore_params
    underscore_params=$(grep -rEn "([(,]\s*_[a-z][a-z_]*\s*:)" src/ 2>/dev/null \
        | grep -v "test\|mock\|Mock" \
        || true)

    if [[ -n "$underscore_params" ]]; then
        if echo "$underscore_params" | grep -q "src/main\.rs"; then
            echo "Quality gate FAILED: main.rs has underscore params — must use them"
            echo "$underscore_params"
            issues_found=1
        fi
    fi

    return $issues_found
}

for i in $(seq 1 $MAX_ITERATIONS); do
    echo ""
    echo -e "${BLUE}===============================================================${NC}"
    echo -e "${YELLOW}  Ralph Iteration $i of $MAX_ITERATIONS${NC}"
    echo -e "${BLUE}===============================================================${NC}"

    # Auto-commit any uncommitted changes from previous iteration
    if [[ -n "$(git status --porcelain)" ]]; then
        echo -e "${YELLOW}Auto-committing changes from previous iteration...${NC}"
        git add -A
        git commit -m "ralph: iteration $((i - 1)) changes" || true
    fi

    # Build prompt: PROMPT.md + progress.txt injected directly
    CONSTRUCTED_PROMPT=$(cat "$PROMPT_FILE")
    if [[ -f "$PROGRESS_FILE" ]]; then
        CONSTRUCTED_PROMPT="${CONSTRUCTED_PROMPT}

---
## Current Iteration: ${i} of ${MAX_ITERATIONS}

## Progress from previous iterations
\`\`\`
$(cat "$PROGRESS_FILE")
\`\`\`"
    fi

    # Run Claude with timeout — capture output
    echo -e "${BLUE}Running Claude (timeout: ${TIMEOUT_MINUTES}m)...${NC}"

    OUTPUT=$(timeout "${TIMEOUT_MINUTES}m" claude \
        --model "$MODEL" \
        --dangerously-skip-permissions \
        --print \
        <<<"$CONSTRUCTED_PROMPT" 2>&1 | tee /dev/stderr) || {
        EXIT_CODE=$?
        if [[ $EXIT_CODE -eq 124 ]]; then
            echo -e "${RED}Timeout! Claude ran for ${TIMEOUT_MINUTES}m without exiting.${NC}"
            echo "Iteration $i: TIMEOUT after ${TIMEOUT_MINUTES}m" >>"$PROGRESS_FILE"
        else
            echo -e "${YELLOW}Claude exited with code $EXIT_CODE${NC}"
        fi
    }

    # Auto-commit any changes made during this iteration
    if [[ -n "$(git status --porcelain)" ]]; then
        echo -e "${YELLOW}Auto-committing iteration $i changes...${NC}"
        git add -A
        git commit -m "ralph: iteration $i complete on $CURRENT_BRANCH" || true
    fi

    # Run validation EXTERNALLY (not inside LLM session) — capture output
    echo -e "${BLUE}Running external validation: $VALIDATION_CMD${NC}"
    VALIDATION_PASSED=false
    VALIDATION_OUTPUT=$(eval "$VALIDATION_CMD" 2>&1 | tee /dev/stderr) && {
        echo -e "${GREEN}Validation PASSED${NC}"
        VALIDATION_PASSED=true
        echo "Iteration $i: PASS" >>"$PROGRESS_FILE"
    } || {
        echo -e "${RED}Validation FAILED${NC}"
        # Append validation output directly to progress.txt — truncate to last 50 lines
        {
            echo "Iteration $i: FAIL — validation errors:"
            echo "$VALIDATION_OUTPUT" | tail -50
        } >>"$PROGRESS_FILE"
    }

    # Check for completion signal in output (must be on its own line)
    PROMISE_FOUND=false
    if echo "$OUTPUT" | grep -qx "$COMPLETION_SIGNAL"; then
        PROMISE_FOUND=true
        echo -e "${GREEN}Completion promise found${NC}"
    fi

    # Only consider completion if BOTH validation passes AND promise found
    if [[ "$VALIDATION_PASSED" == "true" && "$PROMISE_FOUND" == "true" ]]; then
        echo -e "${BLUE}Running quality gate checks...${NC}"
        GATE_OUTPUT=$(check_quality_gates 2>&1 | tee /dev/stderr) && {
            echo ""
            echo -e "${GREEN}===============================================================${NC}"
            echo -e "${GREEN}  BUILD COMPLETE!${NC}"
            echo -e "${GREEN}===============================================================${NC}"
            echo "Completed at iteration $i on branch $CURRENT_BRANCH"
            echo "Completed: $(date) on $CURRENT_BRANCH" >>"$PROGRESS_FILE"
            exit 0
        } || {
            echo -e "${YELLOW}Quality gates FAILED — continuing iterations${NC}"
            # Append gate output directly to progress.txt
            {
                echo "Iteration $i: quality gates FAILED:"
                echo "$GATE_OUTPUT"
            } >>"$PROGRESS_FILE"
        }
    fi

    # If promise found but validation failed, note it
    if [[ "$PROMISE_FOUND" == "true" && "$VALIDATION_PASSED" == "false" ]]; then
        echo -e "${YELLOW}LLM claimed complete but validation failed. Continuing...${NC}"
    fi

    echo -e "${YELLOW}Iteration $i complete. Continuing...${NC}"
    sleep "$SLEEP_BETWEEN"
done

echo ""
echo -e "${RED}Max iterations ($MAX_ITERATIONS) reached without completion.${NC}"
echo "Check $PROGRESS_FILE for status."
exit 1
