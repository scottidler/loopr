# Design Document: Faithful Plan Decomposition

**Author:** Scott Idler + Claude
**Date:** 2026-03-31
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

When a user provides a plan via `--plan` or the chat funnel, the coordinator's
decomposition should be faithful to the plan's structure. Currently the LLM
treats the plan as loose inspiration. Two fixes for two use cases: improve
generation prompts for natural language plans (product fix), and add YAML
manifest support for deterministic automation (E2E/CI fix).

## Problem Statement

### Background

Loopr has two paths to plan creation:

1. **Chat funnel (product path):** User converses with the chat LLM, refines
   ideas into a natural language plan (PRD). This plan is submitted to the
   coordinator for decomposition into Spec -> Phase -> Work items.

2. **Headless CLI (automation path):** `loopr run --plan "..." goal` inserts
   a plan directly, skipping the interview. Used for E2E tests, CI/CD, and
   power users who know exactly what they want.

Both paths feed into the same coordinator decomposition pipeline.

### Problem

The Python todo E2E demonstrated that the coordinator decomposes unfaithfully:

- **Plan specified 3 work items** with explicit dependencies (Work 2 and 3
  depend on Work 1). The coordinator created **8 work items** with almost no
  dependencies.
- **Plan specified a single phase**. The coordinator created overlapping items
  (e.g., separate items for "TodoStore data model" and "CRUD operations on
  TodoStore" when the plan had one item covering both).
- **Missing dependencies** caused parallel execution, merge conflicts, and
  abandoned retries.

This is two problems:

1. **Product bug:** The generation prompts don't tell the LLM to respect
   structure already present in the plan text. If a user carefully negotiated
   a 3-item plan in the chat funnel and the coordinator creates 8 items, that's
   a UX failure.

2. **Automation gap:** For E2E tests and CI, relying on an LLM to faithfully
   parse "Work 1 depends on Work 2" from prose is non-deterministic. Tests
   will flake.

### Goals

- Natural language plans are decomposed faithfully when structure is present
- Automation/E2E has a deterministic path that bypasses LLM decomposition
- Both paths are first-class, not hacks

### Non-Goals

- Changing the Plan -> Spec -> Phase -> Work hierarchy
- Removing the LLM's ability to reason about decomposition
- Requiring structured input for the chat funnel

## Proposed Solution

### Overview

Dual-path approach addressing both use cases:

1. **Prompt faithfulness (Phases 1-4):** Fix generation prompts so the LLM
   respects explicit structure in plan text. This is the product fix - it helps
   all plans regardless of source.

2. **YAML manifest (Phase 5):** Add support for `--plan path/to/manifest.yaml`
   that directly creates Plan/Spec/Phase/Work records in the TaskStore,
   skipping LLM decomposition entirely. This is the automation fix.

### Phase 1: Update generation-spec.pmt

Add faithfulness instruction to the spec generation prompt:

```
IMPORTANT: If the Plan's description already contains a detailed technical
approach with phases and work items, create a Spec that faithfully represents
that structure. Do NOT reinterpret, expand, or restructure. The Plan was
created collaboratively with the user and represents their agreed intent.
Your job is to formalize it, not reinvent it.
```

### Phase 2: Update generation-phase.pmt

Add faithfulness instruction to the phase generation prompt:

```
IMPORTANT: If the Spec's description (inherited from the Plan) already
defines specific phases, create those exact phases. Do NOT split a described
phase into multiple phases, and do NOT merge described phases. The number
and scope of phases should match the plan.
```

### Phase 3: Update generation-work.pmt

The most critical prompt. Add faithfulness instruction:

```
IMPORTANT: If the Phase's description (inherited from the Plan) already
specifies individual Work items (e.g., "Work 1: ...", "Work 2: ..."),
create exactly those Work items with:
- The titles and descriptions as specified
- The dependencies as specified (e.g., "depends on Work 1" means add
  a dependency on the first Work's ID or use batch:0)
- The resource_tags as specified (if file paths are mentioned)
- The acceptance_criteria extracted from the description

Do NOT split a described Work item into multiple smaller items.
Do NOT create Work items that aren't in the plan.
Do NOT remove dependencies that the plan specifies.
You may ONLY add Work items if they are strictly necessary prerequisites
that the plan omitted (e.g., installing dependencies). The plan was agreed
upon with the user. Faithfully execute it.
```

### Phase 4: Include plan text in work generation context

Verify that `build_work_prompt()` in `generation.rs` has access to the
original plan description via the phase -> spec -> plan chain. If not,
thread it through so the LLM can see the full plan context when generating
work items.

### Phase 5: YAML Manifest Support for --plan

**File:** `src/cli/dispatch.rs` (run_headless), `src/daemon/handlers/coordinator.rs`

When `--plan` points to a `.yaml` or `.yml` file, deserialize it directly
into TaskStore records instead of sending raw text to `coordinator.accept_plan`.

**Manifest schema:**

```yaml
goal: "Build a Python command-line todo application..."
plan:
  title: "Python Todo App"
  description: "Core implementation and tests"
  spec:
    title: "Todo App Implementation"
    phases:
      - title: "Python todo app with tests"
        works:
          - key: "todo-model"
            title: "Create todo.py with TodoStore class"
            description: |
              TodoStore manages a list of todo dicts...
            resource_tags: ["todo.py"]
            acceptance_criteria:
              - "TodoStore.add() creates a todo with id, title, done=False"
              - "TodoStore.done() marks the correct todo"
              - "TodoStore.delete() removes the correct todo"
              - "JSON persistence survives reload"
          - key: "cli-entry"
            title: "Create cli.py with argparse CLI"
            description: |
              Subcommands: add, list, done, delete...
            resource_tags: ["cli.py"]
            dependencies: ["todo-model"]
            acceptance_criteria:
              - "cli.py add creates a todo"
              - "cli.py list shows todos"
          - key: "test-suite"
            title: "Create test_todo.py with pytest tests"
            description: |
              Test TodoStore CRUD operations...
            resource_tags: ["test_todo.py"]
            dependencies: ["todo-model"]
            acceptance_criteria:
              - "All pytest tests pass"
```

**Key design: Logical key resolution**

The `key` field is a human-readable string (e.g., `"todo-model"`).
The `dependencies` array references these keys. When the CLI parses the YAML
and inserts records into the TaskStore:

1. Create all records, generating real IDs (`wk-xxxxx`)
2. Build a lookup table: `key -> generated ID`
3. Resolve `dependencies` arrays by replacing keys with real IDs
4. Insert all records in one batch

**CLI detection:**

```rust
// In run_headless, detect file path vs raw text
let plan_content = if plan_text.ends_with(".yaml") || plan_text.ends_with(".yml") {
    PlanInput::Manifest(std::fs::read_to_string(plan_text)?)
} else {
    PlanInput::Text(plan_text.to_string())
};
```

For `PlanInput::Manifest`, the CLI creates all records directly and calls
`coordinator.set_goal` (not `coordinator.accept_plan`). The coordinator wakes
up, sees a fully populated hierarchy, and goes straight to Executing.

For `PlanInput::Text`, the existing `coordinator.accept_plan` flow is used
with the improved generation prompts from Phases 1-4.

**E2E target update:**

`bin/e2e-targets/python-todo.sh` switches from a heredoc plan string to a
YAML file at `bin/e2e-targets/python-todo.yaml`. The `target_plan()` function
returns the file path instead of inline text.

## Alternatives Considered

### Alternative 1: YAML Only (No Prompt Fixes)

- **Description:** Only add YAML manifest, don't fix generation prompts.
- **Pros:** Simpler, one change.
- **Cons:** The product path (chat funnel -> NL plan) still produces
  unfaithful decompositions. Users who don't use YAML get a bad experience.
- **Why not chosen:** Both paths need fixing. Prompt faithfulness is a real
  product bug independent of the E2E test suite.

### Alternative 2: Prompt Fixes Only (No YAML)

- **Description:** Only fix generation prompts, rely on LLM for all
  decomposition including E2E tests.
- **Pros:** Simpler, no new input format.
- **Cons:** E2E tests remain non-deterministic. LLM may still flake on
  dependency mapping. We saw the reviewer ignore explicit prompt instructions
  about acceptance criteria - the coordinator could do the same.
- **Why not chosen:** Automation requires determinism. Prompt engineering alone
  can't guarantee it.

### Alternative 3: Pre-parse Plan Text in Rust

- **Description:** Regex/parse the plan text to extract work items and create
  records directly.
- **Pros:** No new input format for existing E2E scripts.
- **Cons:** Extremely fragile. "Phase One:" vs "Phase 1:" vs "Step 1:" all
  break the parser. Permanently couples the data model to a markdown format.
- **Why not chosen:** If you want structured input, use an actual structured
  format (YAML), not a fragile text parser.

## Technical Considerations

### Dependencies

- Phases 1-4: No code changes, only prompt modifications
- Phase 5: `serde_yaml` crate (already a dependency via config parsing)

### Testing Strategy

- **Phases 1-4:** Re-run `bin/e2e.sh --target python-todo` with the text plan
  and verify improved decomposition (fewer items, correct dependencies).
- **Phase 5:** Unit test: deserialize a sample YAML manifest, verify correct
  Plan/Spec/Phase/Work records with resolved dependencies. Then re-run
  `bin/e2e.sh --target python-todo` with the YAML manifest and verify
  GoalComplete with exactly 3 work items.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM still ignores faithfulness instruction | Med | Med | Phases 1-4 are best-effort; Phase 5 YAML is the deterministic fallback |
| YAML schema is too rigid for complex plans | Low | Low | Schema mirrors the existing domain model; can be extended |
| Two input paths increase maintenance | Low | Low | Text path is the existing code; YAML path is a new deserialize-and-insert |
| Logical key resolution fails on circular deps | Low | Med | Validate DAG before inserting; reject circular dependencies |

## Open Questions

- [ ] Does `build_work_prompt()` already have access to the plan description
      via the phase -> spec -> plan chain, or does it need explicit threading?
- [ ] Should YAML manifest support `validation_commands` per phase, or should
      that stay in `loopr.yml` only?

## References

- Generation prompts: `prompts/generation-spec.pmt`, `generation-phase.pmt`,
  `generation-work.pmt`
- Generation code: `src/agents/generation.rs`
- Plan acceptance handler: `src/daemon/handlers/coordinator.rs:244-413`
- Python E2E showing unfaithful decomposition:
  `~/.local/share/loopr/sessions/20260331T202103/`
