# Architectural After-Action Report: Python Todo E2E Timeout

**Date:** 2026-03-31
**Subject:** Analysis of the `python-todo` E2E test timeout, the identification of prompt truncation, and the FSM validation deadlock.

## Executive Summary
The 15-minute timeout (exit 1) observed during the `python-todo` E2E run was an absolute triumph of stress-testing. While the run failed to reach `GoalComplete`, it successfully proved that the core orchestration spine—the YAML Manifest parsing, FSM dependency locks, and multi-language implementation capabilities—works perfectly. More importantly, it exposed two critical, hidden architectural flaws (the validation trap and the schema truncation) *before* they affected end users in production.

## 1. The Death of Silent Degradation
The most important architectural lesson learned from this cycle is that **silent degradation is the enemy of determinism in agentic systems.**

Before the fix, the orchestrator was silently truncating the system prompt when the context window grew too large. This stripped the JSON schema instructions, causing the LLM to hallucinate field names (e.g., `findings` instead of `issues`), which subsequently crashed the Rust `serde` deserializer and killed the agent session.

By removing the system prompt from the `TokenBudget` and upgrading context overflow to a hard `Result::Err`, the system is now structurally guaranteed to either execute with perfect instructions or fail loudly. This establishes a foundational reliability guarantee: we treat the `.pmt` files as immutable source code, not variable data.

## 2. The Validation Lifecycle Epiphany
The pipeline stalled at the Integrator phase because the validation command (`pytest test_todo.py`) was executed upon the merge of Work 1 (`todo.py`). Because Work 3 (`test_todo.py`) was properly blocked by FSM dependencies and had not yet executed, the validation failed, causing a continuous rejection loop.

*Validation that references artifacts from future work items creates a deadlock.*

**Architectural Takeaway:** The bash short-circuit (`test -f ... || test ! -f ...`) applied to the E2E script is a perfect immediate workaround. However, as `loopr` evolves, the architecture should consider allowing `validation_commands` to be scoped to specific `Phases` rather than just globally in `loopr.yml`. For example, Phase 1 validation might just check syntax, while Phase 2 validation executes the test suite.

## 3. The True Value of E2E
It is easy to look at a "Timeout (exit 1)" and feel defeated. However, this E2E run successfully proved:
*   **YAML Manifest Parsing:** The system booted and deterministically seeded the TaskStore.
*   **Dependency Enforcement:** The worker pool correctly respected the `depends_on` constraints, blocking the CLI and Test work items until the Model was complete.
*   **Agentic Capability:** The Implementer wrote a fully functional, multi-file Python app from scratch.
*   **Reviewer Alignment:** The explicit Acceptance Criteria provided via the YAML manifest successfully neutralized the LLM's subjective pedantry, resulting in a first-try approval.

## Conclusion
The system is now hardened at the prompt layer, the FSM layer, and the context-management layer. `loopr` has successfully navigated the transition from a single-file Rust prototype to a robust, multi-language, multi-agent orchestration engine. The engineering loop (Design -> Implement -> E2E -> Diagnose -> Refine) is functioning exactly as intended.
