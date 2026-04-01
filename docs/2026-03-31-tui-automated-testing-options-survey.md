# Survey of Options: Automated TUI Testing for AI Agents

**Date:** 2026-03-31
**Context:** This document surveys options for enabling an AI agent (like Gemini or Claude) to autonomously build, test, debug, and morph a `ratatui`-based TUI without relying on a human-in-the-loop for visual verification (e.g., screenshots).

The fundamental problem is the "Visual Verification Gap": agents can reason about code and state logic, but cannot natively "see" a terminal render. A bridge is needed to convert visual output into structured data that an LLM can read and assert against.

---

## Option 1: The "Textualization" Approach (Recommended & Native)
Implement the core of the `2026-03-21-tui-testing-strategy.md` design, optimizing the output specifically for an LLM's context window.

*   **How it works:** Use ratatui's `TestBackend` to render the UI into an in-memory buffer. Write a utility function (e.g., `dump_buffer_to_string`) that converts the entire 2D `TestBackend` cell grid into a formatted ASCII/UTF-8 representation of the screen.
*   **The Agent Workflow:** When an agent makes a TUI change, it writes an accompanying test that calls `dump_buffer_to_string()`. The agent runs `cargo test -- --nocapture` to see exactly what the terminal *looks* like printed out in the text stream. The agent can visually verify alignment, borders, and content directly in its standard text interface.
*   **Pros:** Requires zero new dependencies. Super fast (in-memory). Fits perfectly into the existing `otto ci` pipeline. Directly cures the agent's visual blindspot.
*   **Cons:** Does not test real `crossterm` PTY events (resizing, exact key escape sequences) or the asynchronous event loop (`run.rs`).

## Option 2: Snapshot Testing with `insta`
This is a standard industry approach for AI-assisted UI testing, relying on automated diffs.

*   **How it works:** Add the `insta` crate. Write tests that render the `TestBackend` and call `insta::assert_snapshot!(terminal.backend())`.
*   **The Agent Workflow:** When a UI layout changes, the `insta` test fails, generating a `.snap.new` file containing the diff. The agent reads this diff file to see exactly how the visual output changed. If the change is correct, the agent runs `cargo insta accept` to update the baseline snapshot.
*   **Pros:** Catches *every* visual regression automatically. The diffs provide high-signal, line-by-line feedback to the agent on what visually moved.
*   **Cons:** High churn. Any minor intentional layout tweak breaks snapshots, requiring constant baseline updates. Can be brittle across ratatui version upgrades.

## Option 3: Headless Terminal Emulators (`ht` / `ratatui-testlib`)
For true end-to-end (E2E) integration testing of the TUI, bypassing `TestBackend` entirely.

*   **How it works:** Use a crate like `ratatui-testlib` or a standalone headless terminal tool like `ht`. These tools spin up a real pseudo-terminal (PTY), launch the compiled binary (`loopr`) inside it, and capture the raw escape sequences to reconstruct a virtual screen state.
*   **The Agent Workflow:** The agent writes a script (e.g., Python or bash) that runs the headless emulator, sends simulated keystrokes, waits for application state to settle, and dumps the screen state as a JSON or plaintext grid for assertions.
*   **Pros:** Tests the *real* application exactly as the user experiences it, including the async event loop, threading, and IPC daemon connections.
*   **Cons:** Slower, significantly more complex to set up, and highly prone to race conditions (e.g., flakiness when waiting for the daemon to respond before asserting screen state).

## Option 4: AI-Native Tooling (MCP / `ht-mcp`)
The bleeding-edge approach for giving agents native, interactive terminal control.

*   **How it works:** Model Context Protocol (MCP) servers like `ht-mcp` are explicitly designed for AI agents. They provide a standardized API for an agent to "view" and "interact with" a running terminal application in real-time.
*   **The Agent Workflow:** The agent is provided access to the MCP tool. It spawns the TUI and calls a tool like `view_terminal_screen()` to get a structured representation of the UI. It then calls `send_terminal_keys("hello")` to interact, looping interactively.
*   **Pros:** The most natural and dynamic way for an agent to "play" with the TUI interactively, discovering bugs organically.
*   **Cons:** Requires setting up and maintaining MCP infrastructure outside of the core Rust codebase. Heavily dependent on the specific agent runtime's tool-calling capabilities.

---

### Recommendation for Loopr

For immediate impact and seamless integration, **Option 1 (Textualization via TestBackend)** is the highest ROI path. By implementing a utility to dump the `TestBackend` buffer to a readable string format within standard Rust unit tests, the agent gains immediate, native visibility into the UI layout without requiring heavy new infrastructure or human screenshot intervention.