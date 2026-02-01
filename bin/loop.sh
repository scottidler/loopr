#!/usr/bin/env bash
# Loopr Build Loop (Ralph Wiggum Pattern)
# Fresh context each iteration - timeout enforced
# Validation runs OUTSIDE the LLM session
# https://ghuntley.com/ralph/
set -e

# Configuration (override via environment variables)
MAX_ITERATIONS=${MAX_ITERATIONS:-100}
PROMPT_FILE=${PROMPT_FILE:-PROMPT.md}
PROGRESS_FILE=${PROGRESS_FILE:-progress.txt}
MODEL=${MODEL:-opus}
SLEEP_BETWEEN=${SLEEP_BETWEEN:-2}
TIMEOUT_MINUTES=${TIMEOUT_MINUTES:-10}
COMPLETION_SIGNAL="<promise>COMPLETE</promise>"
BRANCH_PREFIX=${BRANCH_PREFIX:-iter}
BASE_BRANCH=${BASE_BRANCH:-$(git branch --show-current)}
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

echo -e "${GREEN}=== Loopr Build Loop (Ralph Wiggum) ===${NC}"
echo "Project:    $PROJECT_ROOT"
echo "Prompt:     $PROMPT_FILE"
echo "Progress:   $PROGRESS_FILE"
echo "Model:      $MODEL"
echo "Timeout:    ${TIMEOUT_MINUTES}m per iteration"
echo "Max:        $MAX_ITERATIONS iterations"
echo "Branching:  ${BASE_BRANCH}-${BRANCH_PREFIX}N"
echo "Validation: $VALIDATION_CMD"
echo ""

# Check prompt file exists
if [[ ! -f "$PROMPT_FILE" ]]; then
    echo -e "${RED}Error: $PROMPT_FILE not found${NC}"
    exit 1
fi

# Initialize progress file if needed
if [[ ! -f "$PROGRESS_FILE" ]]; then
    echo "# Ralph Progress Log" >"$PROGRESS_FILE"
    echo "Started: $(date)" >>"$PROGRESS_FILE"
    echo "Base branch: $BASE_BRANCH" >>"$PROGRESS_FILE"
    echo "---" >>"$PROGRESS_FILE"
fi

for i in $(seq 1 $MAX_ITERATIONS); do
    echo ""
    echo -e "${BLUE}===============================================================${NC}"
    echo -e "${YELLOW}  Ralph Iteration $i of $MAX_ITERATIONS${NC}"
    echo -e "${BLUE}===============================================================${NC}"

    # Create iteration branch (iter0, iter1, ...)
    branch_name="${BASE_BRANCH}-${BRANCH_PREFIX}$((i - 1))"
    echo -e "${BLUE}Setting up branch: $branch_name${NC}"

    # Auto-commit any uncommitted changes from previous iteration
    if [[ -n "$(git status --porcelain)" ]]; then
        prev_branch=$(git branch --show-current)
        echo -e "${YELLOW}Auto-committing changes on $prev_branch...${NC}"
        git add -A
        git commit -m "ralph: auto-commit iteration changes on $prev_branch" || true
    fi

    # For first iteration, start from base branch
    if [[ $i -eq 1 ]]; then
        git checkout "$BASE_BRANCH" 2>/dev/null || true
        git pull --ff-only 2>/dev/null || true
    fi

    # Delete branch if it exists (re-running same iteration)
    if git show-ref --verify --quiet "refs/heads/$branch_name"; then
        echo -e "${YELLOW}Branch $branch_name exists, deleting...${NC}"
        git branch -D "$branch_name"
    fi

    # Create new branch from current state
    git checkout -b "$branch_name"
    echo -e "${GREEN}Now on branch: $branch_name${NC}"

    # Run Claude with timeout - capture output
    echo -e "${BLUE}Running Claude (timeout: ${TIMEOUT_MINUTES}m)...${NC}"

    OUTPUT=$(timeout "${TIMEOUT_MINUTES}m" claude \
        --model "$MODEL" \
        --dangerously-skip-permissions \
        --print \
        <"$PROMPT_FILE" 2>&1 | tee /dev/stderr) || {
        EXIT_CODE=$?
        if [[ $EXIT_CODE -eq 124 ]]; then
            echo -e "${RED}Timeout! Claude ran for ${TIMEOUT_MINUTES}m without exiting.${NC}"
            echo -e "${RED}This means Claude is NOT following Ralph Wiggum pattern.${NC}"
            echo "Iteration $i ($branch_name): TIMEOUT after ${TIMEOUT_MINUTES}m" >>"$PROGRESS_FILE"
        else
            echo -e "${YELLOW}Claude exited with code $EXIT_CODE${NC}"
        fi
    }

    # Auto-commit any changes made during this iteration
    if [[ -n "$(git status --porcelain)" ]]; then
        echo -e "${YELLOW}Auto-committing iteration $i changes...${NC}"
        git add -A
        git commit -m "ralph: iteration $i complete on $branch_name" || true
    fi

    # Run validation EXTERNALLY (not inside LLM session)
    echo -e "${BLUE}Running external validation: $VALIDATION_CMD${NC}"
    VALIDATION_PASSED=false
    if eval "$VALIDATION_CMD"; then
        echo -e "${GREEN}Validation PASSED${NC}"
        VALIDATION_PASSED=true
        echo "Iteration $i: PASS" >>"$PROGRESS_FILE"
    else
        echo -e "${RED}Validation FAILED${NC}"
        echo "Iteration $i: FAIL - validation failed" >>"$PROGRESS_FILE"
    fi

    # Check for completion signal in output (must be on its own line)
    PROMISE_FOUND=false
    if echo "$OUTPUT" | grep -qx "$COMPLETION_SIGNAL"; then
        PROMISE_FOUND=true
        echo -e "${GREEN}Completion promise found${NC}"
    fi

    # Only complete if BOTH validation passes AND promise found
    if [[ "$VALIDATION_PASSED" == "true" && "$PROMISE_FOUND" == "true" ]]; then
        echo ""
        echo -e "${GREEN}===============================================================${NC}"
        echo -e "${GREEN}  BUILD COMPLETE!${NC}"
        echo -e "${GREEN}===============================================================${NC}"
        echo "Completed at iteration $i on branch $branch_name"
        echo "Completed: $(date) on $branch_name" >>"$PROGRESS_FILE"
        exit 0
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
