#!/usr/bin/env bash
# =============================================================================
# ensure-tests-exist.sh - Fail fast if no tests are wired into the crate
# =============================================================================
set -e
set -o pipefail

test_output=$(cargo test --all-targets --workspace -- --list --format=terse 2>&1) || {
    echo "$test_output"
    echo ""
    echo "Build or test discovery failed!"
    exit 1
}

if ! echo "$test_output" | awk 'END { exit(found ? 0 : 1) } /: test$/ { found=1 }'; then
    echo "$test_output"
    echo ""
    echo "No tests found! Tests must be wired into the crate."
    exit 1
fi

test_count=$(echo "$test_output" | grep -c ": test$" || echo "0")
echo "Found $test_count tests"
