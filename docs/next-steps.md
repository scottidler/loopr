# Loopr Project Next Steps & Roadmap

This document outlines the major outstanding projects for Loopr, ordered by precedence. These items represent the remaining gaps between the current implementation and a fully hardened, autonomous "dev team in a box."

## ~~1. First Autonomous E2E Run (The Proving Task)~~ COMPLETE (2026-03-30)
**Primary Links:**
- [2026-03-30-first-end-to-end-run.md](design/2026-03-30-first-end-to-end-run.md) (supersedes 2026-03-21 version)
- [2026-03-21-oracle-knowledge-extraction-next-steps.md](2026-03-21-oracle-knowledge-extraction-next-steps.md)

**Result:** GoalComplete. The full Chat -> Plan -> Coordinator -> Implementer -> Reviewer -> Integrator -> GoalComplete pipeline completed autonomously against a disposable `/tmp/loopr-e2e-target` scaffold repo. The Implementer added a `--version` flag in 4 iterations, the Reviewer approved, and the Integrator merged. 10 bugs were fixed during the fix-forward process. Automated via `bin/e2e.sh`.

## 2. The "Heavy" Runner Lane Architecture
**Primary Links:**
- [v2-light-loops-heavy-tools.md](v2-light-loops-heavy-tools.md)
- [2026-03-04-native-tool-use.md](design/2026-03-04-native-tool-use.md)

Loopr's core insight is separating light LLM loops from heavy filesystem/build tools to ensure isolation, sandboxing, and killability.
*   **Current:** Tools (like `cargo build`, `read`, `write`) are executed in-process via `tokio::process` directly from the daemon. A runaway build or blocking tool can hang the entire daemon.
*   **Next:** Implement the three specialized, slot-limited OS subprocess runners (`local`, `net`, and `heavy`). Wire the `ToolRouter` to dispatch tool calls to these runners over Unix sockets, complete with process-group kill mechanisms (`killpg()`) and network sandboxing.

## 3. Semantic Bubble-Up Logic
**Primary Links:**
- [2026-03-03-semantic-decomposition.md](design/2026-03-03-semantic-decomposition.md)
- [2026-03-21-coverage-bubble-up-and-headless-mode.md](design/2026-03-21-coverage-bubble-up-and-headless-mode.md)
- [remaining-gaps.md](design/remaining-gaps.md)

The system needs the ability to self-correct when higher-level plans produce inadequate or incomplete lower-level tasks.
*   **Current:** The `Coordinator` successfully increments `decomposition_attempts` when coverage evaluation fails. However, when the maximum attempts are reached, it simply signals a `need_help` escalation.
*   **Next:** Implement the full `ReviseParent` action. When a child's coverage fails repeatedly, the system should transition the parent (e.g., a `Spec`) back to `Draft`, appending diagnostic feedback of the gaps, and regenerate the parent automatically.

## 4. Pipeline Hardening: Locks & Timeouts
**Primary Links:**
- [2026-03-01-file-touch-broadcasting.md](design/2026-03-01-file-touch-broadcasting.md)
- [remaining-gaps.md](design/remaining-gaps.md)
- [2026-02-27-audit-fixes.md](design/2026-02-27-audit-fixes.md)

Several low-level orchestration safety mechanisms identified during audits remain unimplemented.
*   **Current:** `write.rs` and `edit.rs` tools modify files without advisory locks. Agent tasks do not have hard wall-clock bounds.
*   **Next:**
    - Implement file-touch advisory lock auto-acquisition before file modifications, with guaranteed lock cleanup on agent exit.
    - Wrap `run_agent_task` in a strict `tokio::time::timeout` utilizing the existing `session_timeout_secs` config.
    - Add BFS/DFS acyclic dependency checks for Work items.

## 5. Decompose the Handlers Monolith
**Primary Links:**
- [2026-03-21-codebase-evaluation.md](2026-03-21-codebase-evaluation.md)

Technical debt in the daemon RPC layer threatens maintainability.
*   **Current:** `src/daemon/handlers.rs` is a single file containing over 13,000 lines of code, handling every IPC dispatch in the system.
*   **Next:** Semantically decompose the monolith into specialized, modular files (e.g., `chat_handlers.rs`, `loop_handlers.rs`, `plan_handlers.rs`) to improve readability and reduce merge conflicts.

## 6. Nightly E2E Persona Test Automation
**Primary Links:**
- [2026-03-30-chat-funnel-test-refactor.md](design/2026-03-30-chat-funnel-test-refactor.md)
- [2026-03-29-conversational-funnel-testing.md](design/2026-03-29-conversational-funnel-testing.md)

Prevent prompt regressions and verify the quality of the chat-to-orchestration bridge.
*   **Current:** The multi-turn LLM persona tests in `tests/funnel.rs` were successfully rewritten for the Chat Funnel but are marked `#[ignore]` because they require real LLM API calls, making them too slow and flaky for the standard CI loop.
*   **Next:** Establish a dedicated nightly CI job (e.g., `otto e2e-nightly`) that runs these ignored tests against real Anthropics endpoints to track structural planning degradation over time.
