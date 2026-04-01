# Design Document: Large File Decomposition

**Author:** Scott A. Idler
**Date:** 2026-04-01
**Status:** Draft
**Review Passes Completed:** 1/5

## Summary

Decompose 12 Rust source files that exceed 1,500 lines into focused submodule directories. The two largest files (`executor.rs` at 5,194 lines and `coordinator.rs` at 4,993 lines) each contain single functions exceeding 700 lines, making them the primary velocity and context-window hazard. The refactor is purely structural - zero behavioral changes, `otto ci` passes at every step.

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
- Introducing new traits, abstractions, or design patterns (beyond what's required to pass context cleanly between split modules)
- Changing the public API of any module
- Addressing the `.unwrap()` audit or other code quality issues
- Refactoring the FSM logic itself, only its file location

## Proposed Solution

### Overview

Convert each oversized file into a module directory using the existing `mod.rs` pattern already established in this codebase. Each logical concern becomes a single-word file. Shared state that previously lived implicitly in the same closure is extracted into a small context struct where necessary.

### Execution Priority

The architect's assessment validated this order:

1. **Test files first** (`integration_tests.rs`, `fsm_correctness_tests.rs`) - zero production coupling, zero risk, removes ~5,800 lines from the monolith immediately.
2. **`executor.rs`** - highest line count in production code; `execute_action()` is the immediate velocity bottleneck.
3. **`coordinator.rs`** - second-highest, but more complex interdependencies; do after executor as a warm-up.
4. **Remaining files** - each independently decomposable in any order; tackle from largest to smallest.

### Critical Design Decision: ExecutionContext Struct

`execute_action()` in `executor.rs` is a 1,334-line single function. When split into per-domain action handlers (`action/tool.rs`, `action/file.rs`, etc.), each handler needs access to: `Stores`, `AgentContext`, `EventTx`, `WorktreeManager`, and the `AgentAction` being processed. Without a bundling struct, every handler signature becomes a 6-7 argument list prone to Rust lifetime errors.

The solution is a small, non-opinionated context struct:

```rust
// src/agents/executor/context.rs
pub(super) struct ExecutionContext<'a> {
    pub stores: &'a Arc<Stores>,
    pub agent_ctx: &'a AgentContext,
    pub event_tx: &'a EventTx,
    pub worktree_mgr: &'a WorktreeManager,
}
```

Each action handler takes `ctx: &ExecutionContext<'_>` plus the specific action variant's fields. This is the only new type introduced by this refactor.

### Module Structures

#### `src/agents/executor/` (from 5,194 lines)

```
agents/executor/
  mod.rs              # run_single_work(), entry points, re-exports
  lifecycle.rs        # run_agent_task(), run_agent_loop()
  context.rs          # ExecutionContext<'a> struct
  result.rs           # ActionResult enum
  util.rs             # work_handback, resolve_*, normalize_*, auto_acquire_write_lock, persist_session
  llm.rs              # create_llm_client()
  action/
    mod.rs            # execute_action() dispatcher - routes to submodules
    tool.rs           # RunTool, RegisterTool
    file.rs           # WriteFile, EditFile, ReadFile
    work.rs           # CreateWork, AssignAgent, TransitionWork
    bundle.rs         # ProposeBundle, OverrideBundle, TransitionBundle
    record.rs         # CreatePlan, CreateSpec, CreatePhase, TransitionRecord
    lock.rs           # AcquireLock, ReleaseLock
    learning.rs       # CreateLearning
    validation.rs     # ValidateDocument, EvaluateCoverage
```

#### `src/agents/coordinator/` (from 4,993 lines)

```
agents/coordinator/
  mod.rs              # CoordinatorAgent struct, Agent impl, re-exports
  loop.rs             # run_fsm_loop(), run_iteration()
  util.rs             # infer_action_level(), format_action_summary(), last_error_kind_for_work()
  state/
    mod.rs            # CoordinatorState helpers, re-exports
    fsm.rs            # check_fsm_transition() (689 lines), apply_fsm_transition()
    persistence.rs    # load_or_create_coordinator_state(), persist_coordinator_state()
    cleanup.rs        # sweep_integrated_to_done()
  context/
    mod.rs            # re-exports
    summary.rs        # build_state_summary_with_sla() (951 lines)
    learning.rs       # query_learnings_for_level()
  generation/
    mod.rs            # re-exports
    footer.rs         # build_generation_footer()
    fsm_footer.rs     # build_fsm_footer()
    validation.rs     # find_pending_draft_for_validation(), validation cap logic
  phase/
    mod.rs            # re-exports
    gate.rs           # phase gate logic extracted from check_fsm_transition
    completion.rs     # check_phase_completion(), mark_phase_record_complete()
    activation.rs     # find_next_phase_to_activate()
    status.rs         # build_phase_status()
    tools.rs          # phase_missing_test_tool()
  dependencies/
    mod.rs            # re-exports
    resolve.rs        # resolve_batch_dependencies()
    prune.rs          # prune_independent_deps()
```

#### `src/tests/` (from `integration_tests.rs`, 3,705 lines)

```
tests/
  mod.rs              # submodule declarations
  fixtures.rs         # test_stores(), test_agent_logger(), dispatch_ok/err, create_test_hierarchy()
  hierarchy.rs        # hierarchy creation & lifecycle (tests 1-2)
  learning.rs         # learning auto-promotion, contradiction (tests 3-4)
  pool.rs             # pool exhaustion, terminal cleanup (tests 5-6)
  coordinator.rs      # coordinator state, goals, generation progression (tests 7, 16, 18, 27-30)
  tick.rs             # tick crash recovery, lifecycle (tests 8, 20)
  locks.rs            # lock lifecycle (test 10)
  context.rs          # role filtering, path sandboxing, role inference (tests 9, 11-12)
  fsm.rs              # FSM enforcement (test 13)
  sessions.rs         # multi-agent coexistence, pause/resume (tests 14-15, 17)
  config.rs           # strategy knobs, defaults, dispatch routes (tests 19-20)
  pipeline.rs         # full pipeline end-to-end (tests 21-23)
  executor.rs         # coordinator-via-executor integration (tests 24-26)
  cycling.rs          # FSM cycling, dependency chains, duplicate rejection (tests 31-34)
  errors.rs           # error handling, lifeguard, max requeries (tests 35-37, 41-43)
  advisory.rs         # advisory review flows (tests 38-40)
  preformed.rs        # preformed plan injection tests (tests 44-46)
```

#### `src/fsm_tests/` (from `fsm_correctness_tests.rs`, 2,122 lines)

```
fsm_tests/
  mod.rs              # submodule declarations
  common.rs           # ALL_ROLES, assert_valid/assert_invalid macros, shared setup
  hierarchy.rs        # HierarchyStatus FSM (10 tests)
  work.rs             # WorkStatus FSM (26+ tests)
  bundle.rs           # BundleStatus FSM (21+ tests)
  tick.rs             # TickStatus FSM (17+ tests)
  lock.rs             # LockStatus FSM (15 tests)
  agent_status.rs     # AgentStatus FSM (14+ tests)
  dispatch.rs         # IPC dispatch lifecycle tests (9 tests)
```

#### `src/tui/run/` (from `src/tui/run.rs`, 2,552 lines)

```
tui/run/
  mod.rs              # run_tui(), draw(), role_actions() - public API
  terminal.rs         # restore_terminal(), try_connect(), RECONNECT_INTERVAL, FRAME_INTERVAL
  events.rs           # extract_llm_chunk(), extract_tool_event(), process_ipc_message(), handle_daemon_event(), format_orchestration_event()
  ipc.rs              # dispatch_ipc_action(), refresh_collection(), event_collection()
  ui.rs               # render_header(), render_content(), render_footer(), draw_goal_input(), draw_help_overlay()
```

#### `src/agents/generation/` (from 2,293 lines)

```
agents/generation/
  mod.rs              # GenerationLevel enum, re-exports
  prompts.rs          # build_plan_prompt(), build_spec_prompt(), build_phase_prompt(), build_work_prompt()
  hierarchy.rs        # determine_generation_level(), find_active_*(), is_phase_complete()
  validation.rs       # RegenerationInfo, find_draft_needing_regeneration(), is_validation_cap_reached()
  coverage.rs         # CoverageCheckNeeded, IncompleteDecomposition, gap queries, is_decomposition_cap_reached()
```

#### `src/agents/integrator/` (from 2,163 lines)

```
agents/integrator/
  mod.rs              # IntegratorAgent, Agent impl re-exports
  core.rs             # IntegratorAgent struct, new(), run() main loop
  tick_state.rs       # latest_published_tick_id(), next_tick_number(), has_tick_in_progress()
  recovery.rs         # recover_stuck_ticks(), reset_work_after_bundle_rejection()
  cycle.rs            # IntegratorCycleResult, run_cycle() (~700 lines)
  integration.rs      # merge_bundle_branches(), tick/bundle state transitions, work updates
  validation.rs       # effective_validation_commands(), run_validation_commands()
  git_ops.rs          # get_git_head_sha()
```

#### `src/agents/implementer/` (from 2,048 lines)

```
agents/implementer/
  mod.rs              # re-exports
  llm.rs              # LlmClient trait, ChatMessage, chat helpers
  parsing.rs          # parse_actions(), strip_markdown_fences(), normalize_action_keys()
  errors.rs           # is_correctable_error(), error classification
  summary.rs          # build_implementer_summary(), context assembly
  lifecycle.rs        # ImplementerAgent struct, Agent impl, run() main loop
  iteration.rs        # run_iteration(), action execution, self-correction loop
  events.rs           # drain_tick_published(), staleness detection
  output.rs           # truncate_content(), format_action_summary()
```

#### `src/agents/types/` (from `agents/mod.rs`, 1,805 lines)

```
agents/mod.rs         # pub trait Agent, re-exports from submodules
agents/types/
  mod.rs              # re-exports
  agent_type.rs       # AgentType enum, default_role(), is_thinking_plane()
  status.rs           # AgentStatus enum, can_transition_to(), is_terminal()
  session.rs          # AgentSession struct, Record impl
  action.rs           # AgentAction enum (40+ variants)
agents/event.rs       # AgentEvent enum
agents/context_state.rs  # AgentContext struct, impl (extracted from agents/mod.rs)
```

Note: The existing `agents/context.rs` (1,691 lines) is a different file - the `ContextBuilder` and token budget system.

#### `src/agents/context/` (from `agents/context.rs`, 1,691 lines)

```
agents/context/
  mod.rs              # re-exports: ContextBuilder, TokenBudget, AssembledContext
  learning.rs         # select_learnings()
  token.rs            # estimate_tokens(), truncate_prose(), truncate_from_head(), truncate_list()
  budget.rs           # TokenBudget struct, for_role()
  assembled.rs        # AssembledContext struct
  builder.rs          # ContextBuilder struct, setters, loaders (new, with_*, load_*)
  section.rs          # section helpers extracted from build(): hierarchy, bundle, learnings, tools sections
```

#### `src/cli/dispatch/` (from 1,591 lines)

```
cli/dispatch/
  mod.rs              # run(), connection dispatch, SetRole/Run special cases
  runner.rs           # run_headless(), polling loop, clarity gate evaluation
  mappers/
    mod.rs            # command_to_ipc() router
    crud.rs           # crud_to_ipc() shared CRUD mapping
    bundle.rs         # bundle_to_ipc()
    tick.rs           # tick_to_ipc()
    worktree.rs       # worktree_to_ipc()
    learning.rs       # learning_to_ipc()
    agent.rs          # agent_to_ipc()
    coordinator.rs    # coordinator_to_ipc()
    lock.rs           # lock_to_ipc()
```

#### `src/daemon/handlers/bundle/` (from 1,559 lines)

```
daemon/handlers/bundle/
  mod.rs              # public API, re-exports
  create.rs           # handle_bundle_create()
  get.rs              # handle_bundle_get()
  list.rs             # handle_bundle_list()
  transition.rs       # handle_bundle_transition()
  update.rs           # handle_bundle_update()
  validators/
    mod.rs            # re-exports
    staleness.rs      # find_latest_published_tick(), staleness guard
    precondition.rs   # one-accepted-per-work, lock ownership, terminal work checks
    policy.rs         # bundle size policy enforcement (max_files_touched, max_loc_changed)
    fsm.rs            # FSM role-based transition validation
```

### Visibility Strategy

Consistent with the handlers decomposition:

- Handler/action functions: `pub(super)` - visible within the parent module, not outside
- Context structs passed between module layers: `pub(super)` or `pub(crate)` depending on reach
- Only the existing public API surface of each module stays `pub`
- Test helpers: `#[cfg(test)] pub(crate) mod tests` in the top-level `mod.rs` of each module; submodules import via `crate::` path

### Implementation Plan

**Phase 0: Test files (zero production risk)**
1. Move `integration_tests.rs` content into `src/tests/` submodule - split by topic, extract `fixtures.rs`
2. Move `fsm_correctness_tests.rs` content into `src/fsm_tests/` submodule - split by FSM
3. Verify `otto ci` passes

**Phase 1: executor.rs**
1. Create `agents/executor/` directory, rename `executor.rs` to `executor/mod.rs`
2. Extract `ActionResult` to `result.rs`
3. Extract `ExecutionContext<'a>` to `context.rs`
4. Extract lifecycle functions to `lifecycle.rs`
5. Extract utilities to `util.rs` and `llm.rs`
6. Create `action/mod.rs` with the `execute_action()` dispatcher
7. Extract each action domain to its `action/*.rs` file (tool, file, work, bundle, record, lock, learning, validation)
8. Verify `otto ci` at each step

**Phase 2: coordinator.rs**
1. Scaffold `agents/coordinator/` directory
2. Extract `util.rs` (pure helpers, no deps)
3. Extract `state/` submodule (persistence, cleanup, then fsm)
4. Extract `context/` submodule (learning first, then summary)
5. Extract `generation/` submodule
6. Extract `phase/` submodule (gate extracted from fsm.rs)
7. Extract `dependencies/` submodule
8. Extract `loop.rs`
9. Verify `otto ci` at each step

**Phase 3: Remaining files (largest-to-smallest)**
- `tui/run.rs` - terminal, events, ipc, ui
- `agents/generation.rs` - prompts, hierarchy, validation, coverage
- `agents/integrator.rs` - core, tick_state, recovery, cycle, integration, validation, git_ops
- `agents/implementer.rs` - llm, parsing, errors, summary, lifecycle, iteration, events, output
- `agents/mod.rs` - types/ subdir + event.rs + context_state.rs
- `agents/context.rs` - learning, token, budget, assembled, builder, section
- `cli/dispatch.rs` - runner + mappers/ subdir
- `daemon/handlers/bundle.rs` - CRUD files + validators/ subdir

Each file is an independent extraction: scaffold directory, copy `mod.rs`, extract one file at a time, verify CI.

### Migration Safety

- Every extraction produces a standalone passing commit
- No file is touched until its predecessor passes `otto ci`
- The `run_cycle()` 700-line function in `integrator.rs` is left intact in `cycle.rs` - it's dense orchestration that needs a follow-up design doc if further decomposition is warranted
- `check_fsm_transition()` in coordinator: extract the phase gate logic into `phase/gate.rs` but leave the core FSM logic in `state/fsm.rs` intact - do not refactor the logic itself

## Alternatives Considered

### Alternative 1: Keep files, use `#[path]` attribute splitting

- **Description:** Use `#[path = "executor_action.rs"] mod action;` to split a file without moving it.
- **Pros:** Preserves `git blame` history on the original file.
- **Cons:** Non-standard Rust, confuses rust-analyzer. The split files are invisible to standard module navigation. Doesn't fix the context-window problem since the original path still exists as a 5K file.
- **Why not chosen:** The `mod.rs` + directory pattern is the idiomatic Rust approach and what this codebase already uses.

### Alternative 2: Trait-based action dispatch in executor

- **Description:** Define an `ActionHandler` trait with one impl per action domain; use dynamic dispatch from `execute_action()`.
- **Pros:** Cleaner separation; new actions require only a new impl.
- **Cons:** Introduces `dyn` dispatch overhead; changes calling convention; turns a structural refactor into a design refactor. Higher risk.
- **Why not chosen:** This refactor is explicitly structural. Logic and signatures are unchanged.

### Alternative 3: Macro-based section markers

- **Description:** Add `// SECTION: action-tool` markers and a custom build script to split at build time.
- **Pros:** Zero file structure changes; history preserved.
- **Cons:** Non-portable, fragile, band-aid. Doesn't fix merge conflicts, language server load, or LLM context exhaustion.
- **Why not chosen:** Doesn't address the root causes.

## Technical Considerations

### Dependencies

No new external dependencies. All changes are internal module reorganization within the existing crate.

### Performance

Zero runtime impact. All function calls are static and monomorphized. Module boundaries are a compile-time concept only.

### Security

No security implications.

### Testing Strategy

- `otto ci` after every single extraction step (non-negotiable gate)
- `cargo test` with full output after each phase completes
- External test files (`integration_tests.rs`, `fsm_correctness_tests.rs`) are the first files touched - if they break, the phase is blocked

### Rollout Plan

Single branch (`refactor/large-file-decomposition`). Phases 0-3 merge sequentially. No feature flags needed.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Lifetime errors when extracting execute_action into per-domain handlers | High | Medium | Introduce ExecutionContext<'a> up front in Phase 1 Step 3; validate with cargo check before splitting |
| Macro visibility (try_handler!, assert_valid) across submodules | Medium | Low | Define macros in parent mod.rs before submodule declarations |
| Test helper visibility across sibling submodules | Medium | Low | Place shared helpers in #[cfg(test)] pub(crate) mod tests in mod.rs |
| check_fsm_transition split breaks coordinator correctness | Medium | High | Move code only; extract phase gate as a called helper, do not rewrite logic |
| Merge conflicts with concurrent agent work during refactor | Low | Medium | Run Phase 0-1 on a quiet window; coordinate with active work items |
| git blame history loss | Low | Low | Accepted cost; use git log --all -- <original path> to recover |

## Open Questions

- [ ] Should `run_cycle()` in `integrator/cycle.rs` (~700 lines) be further decomposed in this pass, or deferred to a follow-up?
- [ ] Should `check_fsm_transition()` in `coordinator/state/fsm.rs` (~689 lines) have the phase gate sub-logic extracted in this pass, or is moving the file sufficient?
- [ ] For the `agents/mod.rs` split, should `AgentContext` move to `agents/context_state.rs` (alongside the file) or to `agents/types/context.rs` (alongside the other types)?
- [ ] `integration_tests.rs` declares test helpers used by `fsm_correctness_tests.rs` - need to verify the dependency direction before splitting.

## References

- [2026-03-31-handlers-decomposition.md](2026-03-31-handlers-decomposition.md) - Prior art for this exact pattern
- [2026-03-21-codebase-evaluation.md](2026-03-21-codebase-evaluation.md) - Original assessment flagging these files
- [CLAUDE.md](../../CLAUDE.md) - Single-word filenames, module decomposition rules
