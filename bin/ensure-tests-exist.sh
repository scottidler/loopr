#!/usr/bin/env bash
# =============================================================================
# ensure-tests-exist.sh - Fail fast if no tests are wired into the crate
# =============================================================================
#
# PURPOSE:
#   This script ensures that tests actually exist before running them. A common
#   failure mode is a crate that compiles fine but has zero tests wired up -
#   cargo test happily reports "0 passed" which looks like success. This script
#   catches that case and fails explicitly.
#
#   NOTE: This script only VALIDATES that tests exist. It does NOT run them.
#   The actual test execution should happen after this script succeeds.
#
# WHAT IT CHECKS:
#   1. Build succeeds (cargo test --list compiles the test targets)
#   2. At least one test exists (looks for ": test$" in cargo's test list output)
#
# EXIT CODES:
#   0 - Tests exist and are ready to run
#   1 - Build failed or no tests found
#
# OUTPUT BEHAVIOR:
#   - On SUCCESS: Prints "✅ Found N tests" and exits 0. The full test list is
#     NOT displayed (only the count).
#   - On FAILURE: The captured cargo output IS displayed, followed by an error
#     message, so you can see exactly what cargo reported.
#
# USAGE:
#   bin/ensure-tests-exist.sh && cargo test    # Run directly
#   otto test                                   # Run via otto (calls this script then cargo test)
#
# =============================================================================
set -e

# Capture test list output for validation
# NOTE: This output is intentionally NOT shown on success - only on failure.
# If you want to always see it, change to: cargo test ... 2>&1 | tee /dev/stderr
set -o pipefail
test_output=$(cargo test --all-targets --workspace -- --list --format=terse 2>&1) || {
    echo "$test_output"
    echo ""
    echo "❌ Build or test discovery failed!"
    exit 1
}

# Check that tests actually exist (not just "0 tests")
# The awk script looks for lines ending in ": test" which cargo outputs for each test
if ! echo "$test_output" | awk 'END { exit(found ? 0 : 1) } /: test$/ { found=1 }'; then
    echo "$test_output"
    echo ""
    echo "❌ No tests found! Tests must be wired into the crate."
    exit 1
fi

# Count and report
test_count=$(echo "$test_output" | grep -c ": test$" || echo "0")
echo "✅ Found $test_count tests"
