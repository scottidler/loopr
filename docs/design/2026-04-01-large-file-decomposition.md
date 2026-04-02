# Design Document: Large File Decomposition

**Author:** Scott A. Idler
**Date:** 2026-04-01
**Status:** Approved

## Summary

Twelve Rust source files exceed 1,500 lines and need to be decomposed into focused submodule directories. The two largest (`executor.rs` at 5,194 lines and `coordinator.rs` at 4,993 lines) each contain single functions exceeding 700 lines, making them the primary velocity and context-window hazard.

## Problem Statement

### Background

Loopr's core agent subsystem grew organically from a handful of state machines into a full orchestration engine. As features accumulated, several files crossed the maintainability threshold. The `handlers.rs` decomposition (2026-03-31) demonstrated that this class of refactor is low-risk and high-reward; the same pattern now needs to be applied to the remaining 12 files over 1,500 lines.

### Problem

Twelve files currently exceed 1,500 lines:

| File | Lines | Primary Issue |
|------|-------|---------------|
| `src/agents/executor.rs` | 5,194 | `execute_action()` is a 1,334-line match dispatcher |
| `src/agents/coordinator.rs` | 4,993 | `build_state_summary_with_sla()` is 951 lines; `check_fsm_transition()` is 689 lines |
| `src/integration_tests.rs` | 3,705 | 46+ tests with no topical grouping |
| `src/tui/run.rs` | 2,552 | Five concerns mixed: terminal, events, IPC, rendering, tests |
| `src/agents/generation.rs` | 2,293 | Four independent query/build concerns in one file |
| `src/agents/integrator.rs` | 2,163 | `run_cycle()` is ~700 lines |
| `src/fsm_correctness_tests.rs` | 2,122 | Six FSMs tested in one flat file |
| `src/agents/implementer.rs` | 2,048 | LLM abstraction, parsing, lifecycle, iteration mixed together |
| `src/agents/mod.rs` | 1,805 | Agent types, status FSM, session, 40+ action variants, 370 lines of tests |
| `src/agents/context.rs` | 1,691 | Token math, budget, learning selection, builder, 880 lines of tests |
| `src/cli/dispatch.rs` | 1,591 | Eight command-family mappers plus headless runner in one file |
| `src/daemon/handlers/bundle.rs` | 1,559 | CRUD + business rules + 1,000+ lines of tests |

This causes:

- **Context window exhaustion**: When Loopr's own agents (or Claude Code) work on these files, they consume the majority of the context window just loading the file - then have no room for reasoning.
- **Merge conflicts**: Any two features touching different logical concerns within a file conflict on every diff.
- **rust-analyzer slowness**: Language server must parse and type-check all 5,000 lines every time a single line changes.
- **Cognitive load**: A developer reading `run_cycle()` must mentally filter out 2,000 lines of unrelated tick state and git utilities.
- **Test locality broken**: Tests are separated from the 200-line domain they exercise, making coverage audits by inspection impossible.

### Goals

- All production `.rs` files under 800 lines (target), hard ceiling 1,200 lines
- All test modules co-located with their implementation
- Zero behavioral changes - pure structural refactor
- `otto ci` passes before and after every extraction step
- Each commit is a standalone rollback point

### Non-Goals

- Changing any function signatures or logic
- Introducing new traits, abstractions, or design patterns
- Changing the public API of any module
- Addressing the `.unwrap()` audit or other code quality issues
- Refactoring the FSM logic itself, only its file location
