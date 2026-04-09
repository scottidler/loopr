# Design Document: E2E Post-Mortem Hardening (python-api Run)

**Author:** Scott A. Idler
**Date:** 2026-04-08
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Six targeted fixes addressing failure modes observed in the python-api E2E run. The run completed
successfully (exit 0, docker tests passing) but exposed systemic issues: 4 of 11 work items were
abandoned due to parallel same-file collisions, a false GoalComplete on a mostly-abandoned phase,
cross-module signature mismatches passing review, AC hallucination from the decomposer, scope
rigidity preventing cross-file fixes, and noisy validation warnings on conditional template sections.

## Problem Statement

### Background

The python-api E2E run built a FastAPI bookmarks CRUD API from scratch. 7 of 11 works completed
successfully, 6 bundles merged cleanly, and docker validation passed 3/3 tests. However, the test
suite spec (sp-axw6y) had 5 work items - only 1 (Fixtures) reached Done. The other 4 were abandoned
due to structural merge conflicts, AC hallucination, and blocked dependency chains. The coordinator
declared GoalComplete despite this, and a latent `update_bookmark` argument bug shipped to main
undetected.

### Problems

**P1 - Parallel writers on the same file produce structural merge conflicts**

The decomposer created 5 work items for the test suite phase, all targeting `test_api.py`.
No dependencies were declared between them. They ran in parallel, 2 produced merge conflicts
(wk-tj8kx, wk-ynu1f), and 1 was abandoned transitively (wk-56bk3). The decomposer prompt
(work.pmt line 48) says "Work items that touch different files can always run in parallel" but
says nothing about what to do when multiple works MUST target the same file. The existing
`prune_independent_deps` function (coordinator.rs:394) correctly keeps deps for overlapping files -
but only when the LLM actually declares them. The prompt actively discourages dependencies
("Most work items should have NO dependencies"), so the LLM omits them even when file overlap
makes them necessary.

This was the root cause of 3 of the 4 abandonments.

**P2 - Phase gate treats mostly-abandoned phases as complete**

`is_phase_complete()` (generation.rs:188) considers a phase complete when ALL works are either
Done or Abandoned. The coordinator's `PhaseGate` state (coordinator.rs:1188) then checks for
remaining phases - if none, it transitions to `GoalComplete`. In the E2E run, ph-6pfih (Test Suite
Implementation) was marked complete with 1/5 works Done and 4/5 Abandoned. The coordinator declared
GoalComplete despite the test suite being largely unbuilt. There is no quality gate - a phase with
100% abandoned works would also pass.

**P3 - Reviewer does not catch cross-module signature mismatches**

The implementer for CRUD route handlers (wk-lrung) called `update_bookmark(conn, id, title, url)`
with 4 arguments, but `database.py` defines `update_bookmark(conn, id, title, url, tags)` taking 5.
The reviewer approved this because:
- Its scope is limited to the work's declared files (reviewer.pmt line 4: "your primary job is
  evaluating the code against the Work's acceptance_criteria")
- The AC for the route handler work only required the endpoint to exist, not signature correctness
- The reviewer never reads files outside the work's scope to verify call sites match definitions

The bug shipped to main. Docker tests passed because no test exercised PUT `/bookmarks/{id}`.

**P4 - Decomposer generates AC not grounded in the parent spec**

Work wk-er1lu (Create & Get tests) had AC asserting `created_at` in the API response. The
spec (sp-2o6gz) never defined a `created_at` field. The Plan's contracts section did not include
it. The implementation (wk-lrung) never added it. The implementer for the test correctly could not
satisfy the AC and was abandoned. The decomposer hallucinated a plausible but nonexistent field.

**P5 - Scope rigidity prevents cross-file fixes**

Work wk-56bk3 (Update tests) was abandoned proactively by the coordinator because its dependency
chain was blocked. The root cause: `update_bookmark` in main.py was called with 4 arguments instead
of 5 (missing `tags`). Even if wk-56bk3 had been dispatched, its implementer was scoped to
`test_api.py` only and could not fix the actual bug in `main.py`. Two failure modes compounded:
the blocked dep chain prevented dispatch, and the scope rigidity would have prevented the fix.

The coordinator had no mechanism to:
- Create a quick "fix-first" work targeting main.py
- Then re-dispatch the dependent test work

The current fallback is: abandon, re-plan, re-decompose - which in practice means the work is lost.

**P6 - Validation warnings on conditional sections are noise**

Every spec and many works produced warnings about missing Performance, Open Questions, Alternatives,
and Glossary sections. These are declared as `conditional` in sections.yml (lines 100-117), meaning
they should only appear "when relevant." But the validator warns on every absence, creating log noise
on every run.

### Goals

- Prevent parallel same-file collisions at decomposition time
- Add a quality gate that fails phases with excessive abandonments
- Enable reviewers to detect cross-module signature mismatches
- Ground decomposer AC in parent spec contracts
- Allow the coordinator to create targeted fix-then-retry work sequences
- Silence validation warnings for genuinely optional sections

### Non-Goals

- Rewriting the integrator merge strategy (classify_conflict already works)
- Adding new FSM states (use existing transitions and learnings)
- Full static analysis or type checking of generated code
- Automatic merge conflict resolution

## Proposed Solution

### Phase 1 - Same-File Serialization in Decomposer

**1a. Decomposer prompt update (work.pmt)**

Add an explicit rule after the parallelism section:

```
## Same-File Rule

If two or more Work items target the SAME file, they MUST have sequential dependencies
between them. The first item creates the file; each subsequent item depends on the
previous one. This is non-negotiable - parallel writers on the same file produce
unresolvable merge conflicts.

Order by: scaffolding/structure first, then content additions, then tests that
import from earlier content.

Example: if three Works all write to `test_api.py`:
  - Work A (Fixtures): files=["tests/conftest.py", "tests/test_api.py"], deps=[]
  - Work B (Health tests): files=["tests/test_api.py"], deps=["Fixtures"]
  - Work C (CRUD tests): files=["tests/test_api.py"], deps=["Health tests"]
```

**1b. Coordinator-side safety net: inject deps for overlapping files**

`prune_independent_deps` currently removes deps between disjoint-file works. Add a symmetric
function `inject_overlap_deps` that runs immediately after batch creation:

```rust
fn inject_overlap_deps(stores: &Stores, batch_created_ids: &[String], prefix: &str) {
    // Build file -> Vec<work_id> mapping for works in batch
    // For each file claimed by 2+ works:
    //   Build the existing dep subgraph for those works (may have LLM-declared deps)
    //   For any pair without an existing path between them, add a dep edge:
    //     - Respect existing direction: if LLM declared B -> A, keep it
    //     - Use batch index as tie-breaker only for pairs with no existing relationship
    //   Verify the resulting graph is acyclic (topological sort); if a cycle is detected,
    //   drop the injected edge that caused it and log a warning
    // Persist updated deps
}
```

**Cycle safety:** If the LLM declares B -> A and we naively inject A -> B (because A has a
lower batch index), the result is a cycle and the coordinator deadlocks. The algorithm MUST:
1. Build the existing directed dependency graph for overlapping works
2. Only inject edges where no directed path already exists between the two nodes
3. Use batch index as tie-breaker direction only for disconnected pairs
4. Run cycle detection after injection; drop any edge that creates a cycle

This guarantees sequential execution without introducing deadlocks, even when the LLM declares
reverse-order dependencies.

**Call site:** In coordinator/run.rs, immediately after batch work creation (line 613-615),
call `inject_overlap_deps` BEFORE `prune_independent_deps`:

```rust
if !batch_created_ids.is_empty() {
    inject_overlap_deps(stores, &batch_created_ids, &prefix);   // NEW: add missing deps
    prune_independent_deps(stores, &batch_created_ids, &prefix); // existing: remove false deps
}
```

Order matters: inject first ensures overlapping files get deps, then prune removes any remaining
false deps between non-overlapping files.

### Phase 2 - Phase Quality Gate

**2a. Configurable abandon ratio threshold**

Add `max-abandon-ratio` to the coordinator section of `~/.config/loopr/loopr.yml`:

```yaml
coordinator:
  max-abandon-ratio: 0.4   # default: 0.4 (40%)
```

In `src/config.rs`, add to `CoordinatorConfig`:

```rust
/// Maximum fraction of abandoned works before the quality gate fires.
/// Default: 0.4 (40%). A value of 1.0 effectively disables the gate.
#[serde(default = "default_max_abandon_ratio")]
pub max_abandon_ratio: f64,

fn default_max_abandon_ratio() -> f64 { 0.4 }
```

**Default rationale:** 0.4 (40%) is the smart default. Below 30% is too aggressive - the
coordinator legitimately abandons works during replanning (e.g., replacing a broad work with two
narrower ones). Above 50% allows clearly broken runs to declare success. 40% strikes the balance:
a phase can lose 2 of 5 works (common replanning scenario) but not 3 of 5 (systemic failure).

**2b. GoalComplete quality gate (prompt instruction)**

The FSM auto-transitions to GoalComplete when all phases are exhausted (coordinator.rs:1188-1196).
This transition is code-driven and should NOT be blocked - the coordinator still enters
GoalComplete. The quality gate lives in the coordinator's GoalComplete behavior: it chooses between
`done` (success) and `need_help` (quality failure).

The `max-abandon-ratio` value is interpolated into coordinator.pmt via the existing prompt
interpolation mechanism (same as `{work_status_values}` etc.). Add to the GoalComplete section:

```
Before emitting `done`, count Done vs Abandoned works across all phases:
- If more than {max_abandon_ratio_pct}% of works across ALL phases in the goal are Abandoned,
  do NOT declare done. Instead, emit `need_help` with a summary: "Quality gate failed:
  {done}/{total} works completed, {abandoned}/{total} abandoned. Phases affected: [list].
  Root causes: [list]."
- If {max_abandon_ratio_pct}% or fewer are Abandoned, emit `done` with the work counts in
  the summary.
- Intentional abandonments (works you deliberately replaced with narrower scope) still count.
  The threshold reflects overall goal health, not fault assignment.
```

This keeps the FSM unchanged. The coordinator LLM makes the success/failure judgment with full
context about WHY works were abandoned (structural conflicts vs AC failures vs dep chains).

**2c. Utility function for abandon ratio**

```rust
fn goal_abandon_ratio(stores: &Stores, plan_id: &str) -> f64 {
    let all_works: Vec<_> = stores.read_works().ok()
        .map(|w| w.values()
            .filter(|w| is_descendant_of(stores, &w.parent_id, plan_id))
            .collect())
        .unwrap_or_default();
    let abandoned = all_works.iter().filter(|w| w.status() == WorkStatus::Abandoned).count();
    if all_works.is_empty() { return 0.0; }
    abandoned as f64 / all_works.len() as f64
}
```

This function supports future code-level enforcement if prompt-only proves unreliable. It also
provides the ratio for logging in the GoalComplete handler.

**2d. GoalComplete summary must include abandon counts**

When entering GoalComplete, the coordinator's `done` action summary must include:
- Total works: Done/Abandoned/Other
- Phases with high abandon ratios
- Whether the goal truly succeeded or was a timeout-forced exit

This is a prompt instruction in the GoalComplete section of coordinator.pmt.

### Phase 3 - Cross-Module Signature Verification

**3a. Reviewer prompt enhancement**

Add a new blocking criterion to reviewer.pmt:

```
5. **Cross-Module Calls** - For every function call that targets a module NOT in
   this Work's files, verify the call signature matches the function definition
   visible in the provided context:
   - Full signature available and mismatch detected: `error` (blocking)
   - Signature available but incomplete/multi-line/unparsable: `warning` (non-blocking)
   - Definition not in the provided context: `warning` (cannot verify)
```

**3b. Reviewer context enrichment**

The reviewer already receives the work's file contents via the bundle diff and context. Extend
the reviewer's context assembly (in `src/agents/reviewer.rs` or the function that builds the
review prompt) to also include read-only snippets of imported modules' function signatures:

1. Parse the work's files for import statements
2. For each imported module, if the file exists in the worktree, extract function signatures
   (just the `def`/`fn`/`function` lines, not full bodies)
3. Include these as a "## Referenced Signatures" section in the review context

Python-first implementation: regex for `from X import Y` and `import X`, extract `def name(args):`
lines from the resolved module file. Extend to JS/TS (`function`/`export`) and Rust (`fn`) as
needed - the extraction is language-specific but the context injection is universal.

This gives the reviewer enough information to catch argument mismatches without expanding the
full scope.

### Phase 4 - AC Grounding in Parent Contracts

**4a. Decomposer AC validation rule**

Add to work.pmt:

```
## AC Grounding Rule

Every acceptance criterion MUST be derivable from the parent document's Contracts,
Interfaces, or Outputs sections. Do NOT assert fields, endpoints, or behaviors that
are not explicitly defined upstream. If the parent says the API returns {id, title, url},
do NOT assert {id, title, url, created_at} - that field does not exist.

When writing test ACs, reference the exact response shape from the Contracts section.
```

**4b. Validator cross-reference check (stretch)**

In the doc validator, add an optional check: for each Work's AC, verify that any field names
or endpoint paths mentioned also appear in the ancestor Plan's Contracts section. This is a
heuristic (regex-based, not AST), so it produces warnings rather than errors. Deferred to a
future sprint if prompt-only fix proves sufficient.

### Phase 5 - Coordinator Fix-Then-Retry Pattern

**5a. Scope expansion via coordinator**

When the coordinator sees a Learning indicating a cross-file bug (e.g., "implementer was scoped
to test_api.py but the bug is in main.py"), it should:

1. Create a new "fix" Work targeting the buggy file, parented to the same Phase
2. Set the original (abandoned) work's replacement as depending on the fix Work
3. Create a new replacement test Work (copy of the abandoned one) depending on the fix Work

The coordinator prompt already supports `create_work` with dependencies. Add a rule to
coordinator.pmt:

```
## Cross-File Bug Pattern

When a Learning contains "CROSS-FILE BUG" (created by the implementer or coordinator when
a work is blocked by a defect in a file outside its scope):
1. Create a "fix" Work targeting the buggy file with AC that corrects the specific bug
2. Create a replacement Work (same AC as the original) depending on the fix Work
3. Abandon the stuck original if not already Abandoned
Do NOT expand the original Work's file scope - keep works focused on single concerns.

Learning format: "CROSS-FILE BUG: Work {work_id} blocked by defect in {file}. Details: {description}."

Note: The implementer surfaces this via its existing `need_help` action when it encounters a bug
outside its scope. The coordinator interprets need_help reasons mentioning out-of-scope files as
cross-file bugs and creates the Learning + fix Work sequence.
```

### Phase 6 - Validation Template Noise Reduction

**6a. Update validator prompts to mark conditional sections as optional**

The doc validator is 100% LLM-driven (`src/validator.rs` sends documents to an LLM via prompts
and parses JSON verdicts). There is no Rust code that parses `sections.yml` or iterates over
template sections algorithmically. The validation noise comes from the evaluation criteria in
the validator prompts themselves.

Specifically, `prompts/validator-spec.pmt` criterion 3 states: "Key decisions documented - at
least one alternative was considered and rejected with rationale." This causes the LLM to flag
missing Alternatives/Key Decisions sections even when they are marked `conditional` in
`sections.yml` (which the validator never reads).

**Fix:** Update the evaluation criteria in the validator prompts to explicitly distinguish
required vs optional sections:

- `prompts/validator-spec.pmt`: Soften criterion 3 to: "If key decisions or alternatives are
  documented, they should include rationale. Missing alternatives is acceptable if the approach
  is straightforward."
- `prompts/validator-plan.pmt`: Add a note that Non-Goals, Assumptions, Open Questions, Risks,
  and Success Metrics are optional - their absence should not lower the verdict.
- `prompts/validator-phase.pmt`: No changes needed (phase template has no conditional sections).

The validator prompts are the single source of truth for what the LLM checks. `sections.yml`
defines template structure for the decomposer, not for the validator.

## Alternatives Considered

### Alternative 1: Integrator auto-resolves merge conflicts
- **Description:** Teach the integrator to resolve same-file conflicts by rebasing or
  cherry-picking in sequence
- **Pros:** No decomposer changes needed; handles the problem at merge time
- **Cons:** Rebasing generated code is fragile - the integrator would need to understand
  code semantics, not just text. High complexity, low reliability.
- **Why not chosen:** Prevention (Phase 1) is strictly better than cure for this failure mode

### Alternative 2: Single-file-per-work constraint
- **Description:** Enforce that each work item targets exactly one file
- **Pros:** Eliminates file overlap by construction
- **Cons:** Many real tasks require creating both implementation and test files. Would over-
  decompose trivial changes into artificial units.
- **Why not chosen:** Too restrictive; same-file serialization via deps is sufficient

### Alternative 3: Phase retry instead of quality gate failure
- **Description:** Automatically redecompose and retry the entire phase when abandon ratio
  is too high
- **Pros:** Self-healing without human intervention
- **Cons:** Risk of infinite retry loops; the phase may be fundamentally unimplementable
- **Why not chosen:** Quality gate + Learning lets the coordinator make the retry/escalate
  decision with full context. Automatic retry is Phase 2 of this alternative if the
  coordinator-driven approach proves insufficient.

### Alternative 4: Full AST-based cross-module verification
- **Description:** Parse generated code into an AST and verify all call sites match definitions
- **Pros:** Catches all signature mismatches, not just those the reviewer notices
- **Cons:** Requires language-specific parsers for every target language (Python, JS, Rust, etc.).
  Massive scope increase.
- **Why not chosen:** Reviewer context enrichment (signatures of imported modules) catches the
  common case at far lower complexity. AST analysis is a future investment if needed.

## Technical Considerations

### Dependencies

- Phase 1: `prompts/decompose/work.pmt`, `src/agents/coordinator.rs`
- Phase 2: `src/config.rs` (new field), `src/agents/generation.rs`, `src/agents/coordinator.rs`,
  `src/prompts.rs` (interpolation), `prompts/coordinator.pmt`
- Phase 3: `prompts/reviewer.pmt`, reviewer context builder in `src/agents/`
- Phase 4: `prompts/decompose/work.pmt`
- Phase 5: `prompts/coordinator.pmt`
- Phase 6: `prompts/validator-spec.pmt`, `prompts/validator-plan.pmt`

### Performance

- Phase 1b (inject_overlap_deps): O(W * F) where W = works in batch, F = files per work.
  Negligible for batch sizes of 1-10.
- Phase 3b (reviewer context enrichment): Adds file reads for imported modules. Bounded by the
  number of imports in the work's files, typically 2-5 files. Small additional latency per review.

### Security

No external inputs involved. No security implications.

### Testing Strategy

- Phase 1: Unit test for `inject_overlap_deps` - verify deps are injected for overlapping files
  and not for disjoint ones. E2E: re-run python-api and verify test suite works serialize.
- Phase 2: Unit test for `phase_abandon_ratio` utility function. The quality gate itself is
  prompt-based (coordinator LLM decides `done` vs `need_help`), so E2E validation: run python-api
  and verify the coordinator does NOT emit `done` when most works are abandoned.
- Phase 3: E2E validation - re-run python-api, verify the `update_bookmark` 4-arg bug is caught
  by the reviewer.
- Phase 4: E2E validation - re-run python-api, verify no AC references `created_at`.
- Phase 5: Manual test - create a scenario where a bug exists in a file outside the work's scope,
  verify coordinator creates fix-then-retry sequence.
- Phase 6: E2E validation - run python-api, verify validator does not flag missing Alternatives
  or Performance sections as errors/warnings on spec documents.

### Rollout Plan

Ship in risk-ascending order:

1. **Prompt-only fixes** (Phase 1a + 4a + 5a): zero code risk, immediate value
2. **Validation noise** (Phase 6): small validator change, low risk
3. **Phase quality gate** (Phase 2): prompt instruction + utility function, medium risk
4. **Same-file dep injection** (Phase 1b): new coordinator function, medium risk
5. **Reviewer enrichment** (Phase 3): context builder change, medium-high risk
6. **Validator AC cross-reference** (Phase 4b): stretch goal, deferred

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Same-file deps create long chains that serialize the whole phase | Med | Med | Cap chain length at 5; if more works target the same file, the decomposer should restructure |
| Phase quality gate triggers false positives on intentional abandonments | Low | Med | Coordinator prompt instructs it to distinguish planned abandonments from failure cascades |
| Reviewer context enrichment adds too many tokens | Med | Low | Cap signature extraction to first 50 functions per imported module; truncate if needed |
| Cross-file bug pattern creates unbounded fix chains | Low | Med | Limit to 1 fix Work per original; if the fix itself fails, escalate via need_help |
| AC grounding rule is too restrictive for tests that need derived assertions | Med | Low | Rule says "derivable from" not "literally present in" - the LLM can infer valid assertions from contracts |
| Coordinator prompt grows too long for reliable instruction-following | Med | Med | Monitor prompt token count; if exceeding ~4k tokens, extract rules into a separate reference doc that the coordinator reads on-demand |
| Phase 1 makes structural conflict self-heal (from previous sprint) a dead path for same-file conflicts | Low | Low | Defense in depth is fine - self-heal remains active for non-file-overlap conflicts (binary, submodule, etc.) |

## Open Questions

- [x] Phase 1b: Should `inject_overlap_deps` respect the existing dependency direction (if the
      LLM declared B -> A, keep that order) or always impose creation-order (batch index)?
      **Resolved:** Must respect existing direction. Batch index is tie-breaker only for
      disconnected pairs. Naive imposition creates cycles and deadlocks.
- [x] Phase 2: What is the right default for `max_abandon_ratio`? **Resolved:** 0.4 (40%).
      Below 30% is too aggressive for planned replanning; above 50% allows broken runs to pass.
      Configurable via `~/.config/loopr/loopr.yml` for tuning.
- [x] Phase 3b: How do we extract function signatures from arbitrary languages without a parser?
      **Resolved:** Bounded, best-effort regex for Tier-1 languages (Python: `def`, JS/TS:
      `function`/`export`, Rust: `fn`). Hard caps: max 500 lines read per file, max 20
      signatures extracted, max 5 lines per signature (prevents pulling in function bodies).
      If regex fails to match complex signatures (heavy generics, macros, multi-line decorators),
      extraction silently yields nothing - the system falls back to the Phase 3a reviewer prompt
      (warning: definition not in the provided context). Do NOT introduce tree-sitter or external
      parsing binaries in this sprint.

      **Future consideration:** tree-sitter would give us deterministic, AST-accurate extraction
      across all languages. The tradeoff (C-bound dependency, per-language grammar maintenance)
      is not justified for populating an LLM hint today, but becomes worth it if code quality
      demands grow - e.g., if we add static analysis gates or cross-module type checking beyond
      what the reviewer LLM can catch from context alone. Track as a potential Phase 3 evolution.
- [x] Phase 5: Should the fix Work inherit the abandoned work's parent_id, or be parented to
      the same Phase independently? **Resolved:** Parent to the same Phase (consistent with
      coordinator rule "you MUST assign the new Work to the EXACT SAME parent_id").

## References

- `docs/design/2026-04-08-hardening-sprint.md` - previous hardening sprint (implemented)
- `src/agents/generation.rs:188` - `is_phase_complete()` function
- `src/agents/coordinator.rs:394` - `prune_independent_deps()`
- `src/agents/coordinator.rs:1188` - PhaseGate FSM state
- `src/agents/integrator.rs:1192` - `classify_conflict()`
- `prompts/decompose/work.pmt` - work decomposition prompt
- `prompts/reviewer.pmt` - reviewer prompt
- `prompts/coordinator.pmt` - coordinator prompt
- `docs/templates/sections.yml` - validation template definitions
