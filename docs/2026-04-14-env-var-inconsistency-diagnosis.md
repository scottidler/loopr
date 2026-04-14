# Deep Diagnosis: Env Var Inconsistency in python-api E2E Run
**Date:** April 14, 2026

## 1. The Root Cause (Hallucination during Decomposition)
The inconsistency of the database path environment variable stems from an LLM hallucination during the Decomposition phase, which subsequently triggered a strict multi-agent validation standoff.

1. **The Origin (Correct):** The original Plan (`pl-dne5e.md`) and the Database Layer Spec (`sp-zc18v.md`) correctly instructed the agents to configure the database using the `DATABASE_PATH` environment variable.
2. **The Hallucination (Incorrect):** When the Decomposer broke the Spec down into granular Work items, it hallucinated a context-specific variable name. In `wk-0jluq.md` (the task for `get_db_path`), the acceptance criteria and implementation notes explicitly told the Implementer to use `BOOKMARKS_DB_PATH`.
3. **The Implementation:** The Implementer faithfully executed `wk-0jluq.md`, writing `database.py` to use `os.environ.get('BOOKMARKS_DB_PATH')`. The Reviewer evaluated the code against `wk-0jluq.md`'s Acceptance Criteria and approved it.

## 2. The Chain Reaction & Reviewer Standoff
Later in the run, the Decomposer generated the Work items for the Test Suite (`wk-tucel.md`). Because this Decomposer call referenced the *Spec* (which was correct), `wk-tucel.md` explicitly required the test fixture to mock `DATABASE_PATH`.

This created an impossible paradox for the Implementer:
- The Implementer read `database.py` and saw it expected `BOOKMARKS_DB_PATH`.
- To make the test actually isolate the database, the Implementer wrote the fixture to mock `BOOKMARKS_DB_PATH`.
- The Reviewer agent evaluated the bundle against `wk-tucel.md` and **strictly rejected it**:
  > *"bd-jfwtw: Rejected: ... uses BOOKMARKS_DB_PATH instead of DATABASE_PATH as explicitly required by the acceptance criteria. This mismatch means the fixture may not actually isolate the database."*

The agents entered a loop, unable to resolve the conflict between the existing code and the strict Acceptance Criteria of the test work item. Eventually, the orchestrator abandoned the testing work items (`wk-tucel` and `wk-7uk2q` were marked `Abandoned`).

## 3. The "False Positive" E2E Success
If the test tasks were abandoned, how did the verification script report `10 passed` tests?

This revealed a critical flaw in the bash verification script (`bin/e2e-targets/python-api.sh`) combined with Docker Compose caching:
1. The script runs `if (cd "${TARGET}" && docker compose build 2>&1 | /usr/bin/tail -5); then`.
2. Because Bash pipelines return the exit code of the *last* command (unless `set -o pipefail` is used), any failure in `docker compose build` is masked by `tail -5` succeeding.
3. Because the E2E script runs in `/tmp/loopr/e2e/python-api/latest` (a symlink), Docker Compose uses `latest` as the project name.
4. The build silently failed or was skipped, causing `docker compose run --rm test` to execute against a cached `latest-test` image from a *previous* successful E2E run on the same machine.
5. The cached image contained a perfect 10-test suite, returning success and completely masking the agent's failure to complete the task!

---

## Recommended Mitigations

### 1. Mitigating the Agent Hallucination (The "Constants" Problem)
When decomposing large architectures, "magic strings" (like environment variables, table names, or specific configuration keys) often drift between isolated document generations.
- **Shared Context Dictionary:** Implement a "Global Constraints" or "Project Dictionary" section in the original Plan document. Force the Decomposer to copy this dictionary verbatim into *every* child Spec and Work document.
- **Centralized Config Module:** Rather than hardcoding `os.environ.get('DATABASE_PATH')` in `database.py`, architect the Plan to require a `config.py` file first. Have all subsequent works read from `config.py`. This forces a single source of truth that the Decomposer is less likely to contradict.
- **Cross-Work Validation Prompting:** Add a prompt instruction to the Reviewer: *"If the code deviates from the Acceptance Criteria to match an existing file's implementation, highlight the discrepancy but consider suggesting an update to the original file rather than outright rejecting the current bundle."*

### 2. Mitigating the Orchestrator/CLI False Positives
- **Fix the Bash Pipeline Bug:** Immediately update `e2e-targets/*.sh` verification scripts to use `set -o pipefail` or avoid piping build commands directly into `tail` within conditionals. (e.g., redirect to a log file, then `tail` the log file).
- **Isolate Docker Projects:** Do not run verification scripts from the `latest` symlink. Force the scripts to resolve the symlink to the timestamped directory (e.g., `20260413-214931`), and pass `-p <timestamp>` to `docker compose` to ensure absolute container/image isolation between runs.
- **Fail on Abandoned Works:** The E2E test runner should query the `.taskstore/works.jsonl` and instantly fail the E2E run if any work items are in an `Abandoned` or `Cancelled` state, regardless of whether the bash verification commands happen to pass.