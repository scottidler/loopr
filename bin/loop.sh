#!/usr/bin/env bash
# Loopr Build Loop (Ralph Wiggum Pattern)
# Fresh context each iteration, state preserved in .loopr-progress
# https://ghuntley.com/ralph/
set -euo pipefail

# Configuration (override via environment variables)
MAX_ITERATIONS=${MAX_ITERATIONS:-100}
PROMPT_FILE=${PROMPT_FILE:-PROMPT.md}
PROGRESS_FILE=${PROGRESS_FILE:-.loopr-progress}
MODEL=${MODEL:-opus}
SLEEP_BETWEEN=${SLEEP_BETWEEN:-2}
VALIDATION_CMD=${VALIDATION_CMD:-"otto ci"}

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Get script directory and project root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

cd "$PROJECT_ROOT"

echo -e "${GREEN}=== Loopr Build Loop ===${NC}"
echo "Project:    $PROJECT_ROOT"
echo "Prompt:     $PROMPT_FILE"
echo "Progress:   $PROGRESS_FILE"
echo "Model:      $MODEL"
echo "Validation: $VALIDATION_CMD"
echo "Max:        $MAX_ITERATIONS iterations"
echo ""

# Check prompt file exists
if [[ ! -f "$PROMPT_FILE" ]]; then
    echo -e "${RED}Error: $PROMPT_FILE not found in project root${NC}"
    echo "Create a PROMPT.md file with instructions for Claude."
    exit 1
fi

# Initialize progress file if it doesn't exist
if [[ ! -f "$PROGRESS_FILE" ]]; then
    echo -e "${YELLOW}Initializing progress file...${NC}"
    cat > "$PROGRESS_FILE" << 'EOF'
# Loopr Progress File
# This file tracks state between iterations (Ralph Wiggum pattern)
# Claude reads this at start, updates it at end of each iteration

status: "starting"
iteration: 0
current_phase: null
phases_completed: []
phases_remaining: []  # Will be populated from PROMPT.md on first run
last_action: "initialized"
blockers: []
notes: ""
EOF
fi

iteration=0

while true; do
    ((++iteration))

    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
    echo -e "${YELLOW}=== Iteration $iteration of $MAX_ITERATIONS ===${NC}"
    echo -e "${BLUE}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"

    if [[ $iteration -gt $MAX_ITERATIONS ]]; then
        echo -e "${RED}Max iterations reached. Exiting.${NC}"
        echo -e "${YELLOW}Check $PROGRESS_FILE for current state.${NC}"
        exit 1
    fi

    # Record iteration start time
    start_time=$(date +%s)

    # Run Claude with fresh context
    # PROMPT.md tells Claude to read .loopr-progress first thing
    claude --model "$MODEL" \
        --dangerously-skip-permissions \
        < "$PROMPT_FILE"

    end_time=$(date +%s)
    duration=$((end_time - start_time))
    echo -e "${BLUE}Iteration took ${duration}s${NC}"

    # Check progress file was updated (basic sanity check)
    if [[ ! -f "$PROGRESS_FILE" ]]; then
        echo -e "${RED}Warning: Progress file missing after iteration!${NC}"
        echo -e "${RED}Claude may not be following the protocol.${NC}"
    fi

    # Check for completion marker
    if [[ -f ".loopr-complete" ]]; then
        echo -e "${YELLOW}Completion marker found. Running final validation...${NC}"

        if $VALIDATION_CMD; then
            echo ""
            echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
            echo -e "${GREEN}=== BUILD COMPLETE ===${NC}"
            echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
            echo -e "${GREEN}Finished after $iteration iterations.${NC}"
            echo ""
            echo -e "${BLUE}Completion notes:${NC}"
            cat .loopr-complete
            echo ""
            echo -e "${BLUE}Final progress state:${NC}"
            cat "$PROGRESS_FILE"
            exit 0
        else
            echo -e "${RED}Completion marker exists but validation failed!${NC}"
            echo -e "${RED}Removing marker and continuing...${NC}"
            rm -f .loopr-complete
        fi
    fi

    # Run validation to give feedback for next iteration
    echo ""
    echo -e "${YELLOW}Running validation ($VALIDATION_CMD)...${NC}"
    if $VALIDATION_CMD; then
        echo -e "${GREEN}Validation passed.${NC}"
    else
        echo -e "${RED}Validation failed. Next iteration will address issues.${NC}"
    fi

    echo ""
    echo -e "${YELLOW}Sleeping ${SLEEP_BETWEEN}s before next iteration...${NC}"
    sleep "$SLEEP_BETWEEN"
done
