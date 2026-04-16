# Design Document: Orchestration Hardening

**Author:** Scott Idler / Claude
**Date:** 2026-04-15
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Four hardening improvements identified during the Gemini Architect review of the e2e-orchestration-fixes (v0.1.136). Ordered by urgency: structured replacement intent for the coordinator coherence gate, blocked reason taxonomy for Work status disambiguation, AST-aware context truncation via tree-sitter for agents, and pre-merge semantic validation for the integrator.

## Problem Statement

### Background

The e2e-orchestration-fixes design doc (2026-04-15) shipped four bug fixes. The Architect review identified four areas where the fixes are correct but architecturally fragile or incomplete. These are not new bugs - they are structural weaknesses that will produce failures under LLM phrasing drift, status ambiguity, truncated code context, or semantic merge conflicts.

### Problem

**1. Coherence heuristic fragility.** `validate_action_coherence` (coordinator.rs:610-644) scans the `reason` field of `OverrideWork` actions for substrings like "creating", "replacement", "replacing". A negation guard checks for "no replacement", "not replacing". This couples control flow to unstructured LLM text:

- LLM phrasing drift produces false negatives ("I am substituting this with a new parallel node" - no keywords match)
- New negation phrasings produce false positives ("a replacement would be counterproductive" - not caught)
- The Lifeguard backstop wastes 3 LLM round-trips on false positives before escalating

**2. Blocked status ambiguity.** `WorkStatus::Blocked` is overloaded - it means "waiting on dependency" (auto-resolved by `unblock_dependents`), "exhausted retries" (needs coordinator evaluation via `work.need_help`), or "system fault" (needs operator investigation). Without disambiguation:

- `unblock_dependents` can auto-promote an exhausted-retries work to Ready, bypassing coordinator evaluation
- Telemetry cannot distinguish healthy dependency ordering from chronic failure
- Recovery agents have no signal to route on

**3. Blind context truncation.** The decomposer's Phase 4 fix (v0.1.136) injects file contents with a hard 200-line cutoff. This slices through functions, classes, and module-level definitions. When an LLM sees arbitrarily truncated code it hallucinates the remainder or duplicates logic it assumes is missing. The decomposer is a planning step so the impact is moderate, but the implementer agent also reads file context - and there, truncated code directly causes bad code generation. A structural chunking capability benefits all agents.

**4. Post-merge-only validation.** The integrator merges all bundles then runs validation. When a semantically dependent bundle was excluded (Phase 2 fix from v0.1.136), the remaining bundles may merge cleanly at the git level but fail validation - wasting the merge, triggering a full rollback, and rejecting all bundles in the tick. A lightweight pre-merge check (dry-run merge + build) would catch doomed merges before they pollute the integration branch.

### Goals

- Replace keyword coherence heuristic with structured `requires_replacement: bool` on `OverrideWork`
- Add `blocked_reason` enum field to `Work` record; guard `unblock_dependents` against promoting non-dependency blocks
- Introduce tree-sitter-based AST-aware file chunking for agent context injection
- Add pre-merge dry-run validation to the integrator before committing to the real merge

### Non-Goals

- Splitting `WorkStatus::Blocked` into multiple FSM states (metadata field achieves the same observability without touching the proc macro)
- Full language-server integration (tree-sitter parsing is sufficient for structural chunking)
- Dependency graph analysis between bundles (git conflict detection + validation is sufficient)

## Proposed Solution

| Phase | Fix | Model |
|-------|-----|-------|
| 1 | Structured replacement intent + blocked reason taxonomy | sonnet |
| 2 | AST-aware context truncation via tree-sitter | opus |
| 3 | Pre-merge semantic validation | sonnet |

### Phase 1: Structured Replacement Intent + Blocked Reason Taxonomy

**Model:** sonnet

These are both additive `#[serde(default)]` field changes with no architectural complexity. Combined into one phase because they share the same mechanical pattern: add field, wire it at entry/exit points, update prompts/events, test.

**Part A: Structured Replacement Intent**

1. **`OverrideWork` variant** in `src/agents/action.rs:164-168` - add field:

```rust
OverrideWork {
    work_id: String,
    target_status: String,
    reason: String,
    #[serde(default)]
    requires_replacement: bool,
},
```

Also add to `OverridePhase` (action.rs:169-173) and `OverrideSpec` (action.rs:174-178) - same shape, same potential for incoherent replacements.

2. **`validate_action_coherence`** in `src/agents/coordinator.rs:610-644` - replace keyword heuristic:

```rust
pub(crate) fn validate_action_coherence(actions: &[AgentAction], prefix: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let has_create = actions.iter().any(|a| matches!(a, AgentAction::CreateWork { .. }));

    for action in actions {
        if let AgentAction::OverrideWork {
            work_id,
            requires_replacement,
            ..
        } = action
        {
            if *requires_replacement && !has_create {
                warnings.push(format!(
                    "{} override_work on {} has requires_replacement=true \
                     but no create_work action in payload",
                    prefix, work_id,
                ));
            }
        }
    }
    warnings
}
```

3. **Coordinator prompt** in `resources/agents/coordinator.pmt:42` - update schema:

```
16. `override_work`   {"action": "override_work", "work_id": "...", "target_status": "<WorkStatus>", "reason": "...", "requires_replacement": true|false}
```

Update coherence rule (lines 102-106):
```
If requires_replacement is true but no create_work action is present in the same
array, the system rejects the entire action set and asks you to resubmit.
Set requires_replacement to true when using target_status "Superseded".
Set requires_replacement to false when using target_status "Abandoned".
```

4. **Gate in `src/agents/coordinator/run.rs:307-325`** - no change needed. Already consumes `validate_action_coherence` output.

**Part B: Blocked Reason Taxonomy**

1. **New enum** in `src/domain/work.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockedReason {
    DependencyWait,
    ExhaustedRetries,
    SystemFault,
}
```

2. **`Work` struct** in `src/domain/work.rs:35-59` - add field:

```rust
#[serde(default)]
pub blocked_reason: Option<BlockedReason>,
```

3. **Set `blocked_reason`** at entry points to Blocked:

- **Exhausted retries** (`src/daemon/handlers/work.rs:509-515`): `wi.blocked_reason = Some(BlockedReason::ExhaustedRetries)`
- **Explicit transition** (work.transition handler): `wi.blocked_reason = Some(BlockedReason::DependencyWait)` as default for explicit transitions
- **Orphan recovery** (`src/daemon/context.rs:473-476`): `wi.blocked_reason = Some(BlockedReason::SystemFault)`

4. **Clear `blocked_reason`** on exit from Blocked: `wi.blocked_reason = None`

5. **`unblock_dependents` guard** (`src/daemon/handlers/work.rs:328-376`) - **critical correctness fix**:

```rust
.filter(|w| w.status() == WorkStatus::Blocked)
.filter(|w| w.blocked_reason == Some(BlockedReason::DependencyWait)
          || w.blocked_reason.is_none()) // legacy records without reason
```

Without this, exhausted-retries works get auto-promoted to Ready, bypassing coordinator evaluation.

6. **Event broadcasting** - include `blocked_reason` in `work.need_help` event payload.

7. **TUI display** in `src/tui/views/works.rs` - append reason to Blocked works:

```
[Blocked:exhausted-retries] wk-abc - Implement auth endpoint
[Blocked:dependency-wait]   wk-def - Add integration tests
```

### Phase 2: AST-Aware Context Truncation via Tree-sitter

**Model:** opus

Tree-sitter provides language-aware parsing without requiring a full language server. The `tree-sitter` crate (v0.26.x) has mature Rust bindings. Language grammars are separate crates (`tree-sitter-python`, `tree-sitter-rust`, `tree-sitter-javascript`, etc.) that compile their C parsers at build time via `cc` - standard practice in the Rust ecosystem (helix, difftastic, zed all use this pattern).

**What changes:**

1. **New module** `src/treesitter.rs` - AST-aware chunking:

```rust
use tree_sitter::{Parser, Language};

/// Supported languages for AST-aware chunking.
pub enum SourceLang {
    Python,
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    // Fallback: line-based truncation for unsupported languages
    Unknown,
}

/// Detect language from file extension.
pub fn detect_language(path: &std::path::Path) -> SourceLang { ... }

/// Extract top-level definitions (functions, classes, structs, impl blocks)
/// as a structural summary. Returns a string suitable for prompt injection.
/// For each definition: signature + first docstring/comment line.
/// Full bodies are omitted - the LLM sees the shape, not the implementation.
pub fn structural_summary(content: &str, lang: SourceLang, max_lines: usize) -> String { ... }

/// Truncate file content at the nearest complete top-level node boundary
/// that fits within max_lines. Never cuts mid-function or mid-class.
pub fn truncate_at_boundary(content: &str, lang: SourceLang, max_lines: usize) -> String { ... }
```

Two modes:
- **`structural_summary`**: extracts function/class/struct signatures without bodies. Compact representation for decomposer context. Replaces the current 200-line blind truncation.
- **`truncate_at_boundary`**: returns complete top-level nodes up to the line limit. For the implementer and reviewer agents that need actual code.

**Tree-sitter query patterns** (what top-level nodes to extract):

- **Python**: `function_definition`, `class_definition`, `decorated_definition` - extract the `def`/`class` line plus decorators
- **Rust**: `function_item`, `struct_item`, `enum_item`, `impl_item`, `trait_item` - extract signature up to the opening `{`
- **JavaScript/TypeScript**: `function_declaration`, `class_declaration`, `export_statement`, `lexical_declaration` (for `const fn = ...` patterns)

For `structural_summary`, extract the node's first line (signature) plus any preceding comment/docstring. For `truncate_at_boundary`, include complete node bodies but stop before the node that would exceed `max_lines`.

2. **Dependencies** via `cargo add`:

```
tree-sitter
tree-sitter-python
tree-sitter-rust
tree-sitter-javascript
tree-sitter-typescript
```

3. **Decomposer integration** (`src/daemon/handlers/decomposer.rs:464`) - replace blind truncation in `file_content_section`:

```rust
// Before:
let truncated: String = content.lines().take(200).collect::<Vec<_>>().join("\n");

// After:
let lang = crate::treesitter::detect_language(&full);
let truncated = crate::treesitter::structural_summary(&content, lang, 200);
```

The decomposer sees function signatures and class shapes instead of the first 200 arbitrary lines.

4. **Implementer context** (`src/agents/implementer.rs`) - where the implementer reads file contents for context building, use `truncate_at_boundary` instead of line-based truncation to ensure complete function bodies.

5. **Fallback** - for unsupported file types (YAML, JSON, TOML, SQL, etc.) and for parse errors on malformed source files, fall back to the current line-based truncation. Tree-sitter parsing is only applied to source code files with known grammars that parse successfully. All tree-sitter calls must return `Result` or `Option` - never panic on bad input.

### Phase 3: Pre-Merge Semantic Validation

**Model:** sonnet

The integrator currently merges all bundles into the integration branch, then runs validation (build + test). When validation fails, it resets to the pre-merge SHA and rejects all bundles. This is safe but wasteful - doomed merges consume time, and the rollback + retry cycle delays feedback.

**What changes:**

1. **Dry-run merge** before the real merge in `src/agents/integrator.rs`, after the transition gate filters bundles (Phase 2 fix from v0.1.136) but before the actual merge at `merge_bundle_branches` (integrator.rs:1683):

```rust
/// Cumulative dry-run merge of bundle branches. Merges are tested sequentially
/// (A, then B on top of A, etc.) because that's how the real merge works.
/// Returns the list of bundle_ids whose branches merge cleanly in sequence.
/// Bundles that conflict are excluded and all subsequent bundles are skipped
/// (since their dry-run would be against a different base than the real merge).
fn dry_run_merge(
    repo_path: &std::path::Path,
    bundle_branches: &[(String, String)], // (bundle_id, branch_name)
) -> Result<Vec<String>> {
    let pre_merge_sha = current_sha(repo_path)?;
    let mut clean_ids = Vec::new();

    for (bundle_id, branch) in bundle_branches {
        let output = std::process::Command::new("git")
            .args(["merge", "--no-commit", "--no-ff", branch])
            .current_dir(repo_path)
            .output()?;

        if output.status.success() {
            clean_ids.push(bundle_id.clone());
            // Commit the trial merge so the next bundle merges on top of it
            let _ = std::process::Command::new("git")
                .args(["commit", "--no-edit", "-m", "dry-run"])
                .current_dir(repo_path)
                .output();
        } else {
            tracing::warn!(
                "dry-run merge of bundle {} ({}) would conflict; stopping dry-run",
                bundle_id, branch
            );
            // Abort the failed merge
            let _ = std::process::Command::new("git")
                .args(["merge", "--abort"])
                .current_dir(repo_path)
                .output();
            // Stop here - subsequent bundles can't be tested reliably
            break;
        }
    }

    // Reset to pre-merge state regardless of outcome
    let _ = std::process::Command::new("git")
        .args(["reset", "--hard", &pre_merge_sha])
        .current_dir(repo_path)
        .output();

    Ok(clean_ids)
}
```

The dry-run is cumulative: bundle A is merged, then bundle B is merged on top of A, matching the real merge order. If any bundle conflicts, the dry-run stops - subsequent bundles can't be tested against a different base than the real merge would use.

2. **Integration into tick cycle** - call `dry_run_merge` after the transition gate and before `merge_bundle_branches`. Bundles that fail the dry-run are excluded from the real merge (transitioned back from Integrating to Accepted) and retry in the next cycle. If the dry-run stops early due to a conflict, only the bundles that were tested and passed are included.

3. **Interaction with partial bundle filtering** - the Phase 2 fix (transition gate) already filters bundles that can't transition to Integrating. The dry-run merge is a second filter: bundles that transitioned successfully but would produce git conflicts. The two filters are complementary:
   - Transition gate: catches state-level conflicts (advisory locks, invalid status)
   - Dry-run merge: catches git-level conflicts (file overlap, branch divergence)

4. **The real merge still runs validation** - the dry-run only catches git conflicts. Semantic failures (build errors, test failures) are still caught by post-merge validation. The dry-run reduces wasted merge cycles, not eliminates them.

## Alternatives Considered

### Alternative 1: Split WorkStatus::Blocked into multiple FSM states

- **Description:** Replace `Blocked` with `BlockedByDependency`, `ExhaustedRetries`, `SystemFault`
- **Pros:** Type-safe at the FSM level
- **Cons:** Touches proc macro, every handler, TUI, event consumers, JSONL migration
- **Why not chosen:** Metadata field achieves the same observability without the blast radius

### Alternative 2: Expand keyword negation list for coherence

- **Description:** Add more negation patterns to the keyword matcher
- **Pros:** No schema change
- **Cons:** Cat-and-mouse game; false negatives are silent
- **Why not chosen:** Structured boolean eliminates the entire class of problems

### Alternative 3: Regex-based function boundary detection instead of tree-sitter

- **Description:** Use regex to find function/class definitions and truncate at boundaries
- **Pros:** No new dependencies
- **Cons:** Regex is fragile across languages (Python indentation-based, Rust brace-based, etc.). Multi-line signatures, decorators, and nested definitions break simple patterns.
- **Why not chosen:** Tree-sitter handles all these cases correctly with battle-tested grammars

### Alternative 4: Full file injection instead of truncation

- **Description:** Always inject complete file contents
- **Pros:** Maximum context
- **Cons:** Blows up prompt size for large files. A 2000-line file in the decomposer prompt wastes tokens on implementation details irrelevant to planning.
- **Why not chosen:** Structural summaries (signatures only) are more informative per token than raw code

## Technical Considerations

### Dependencies

- Phase 1: No new dependencies
- Phase 2: `tree-sitter` (0.26.x), `tree-sitter-python`, `tree-sitter-rust`, `tree-sitter-javascript`, `tree-sitter-typescript` via `cargo add`
- Phase 3: No new dependencies

### Performance

- Phase 1: Zero cost (boolean check replaces string scan)
- Phase 2: Tree-sitter parsing is sub-millisecond for typical source files. Parsers are initialized once and reused.
- Phase 3: One additional `git merge --no-commit` + `git merge --abort` per bundle per tick. Milliseconds for typical branches. Net savings when it prevents a doomed full-merge + rollback cycle.

### Backward Compatibility

- Phase 1: `#[serde(default)]` on both new fields. Existing JSONL records and LLM responses work unchanged.
- Phase 2: Fallback to line-based truncation for unsupported languages. Existing behavior preserved when tree-sitter cannot parse a file.
- Phase 3: No data model changes. Purely behavioral - bundles that would have failed post-merge now fail pre-merge instead.

### Testing Strategy

**Phase 1:**
- `OverrideWork` with `requires_replacement: true` and no `CreateWork` -> coherence warning
- `OverrideWork` with `requires_replacement: false` and no `CreateWork` -> no warning
- `OverrideWork` with `requires_replacement: true` and `CreateWork` present -> no warning
- Deserialize `OverrideWork` JSON without `requires_replacement` -> defaults to `false`
- Ready -> Blocked via exhausted retries -> `blocked_reason` is `ExhaustedRetries`
- Orphan recovery -> `blocked_reason` is `SystemFault`
- `unblock_dependents` skips `ExhaustedRetries` works (critical correctness)
- `unblock_dependents` promotes legacy `None`-reason works (backward compat)
- `work.need_help` event includes `blocked_reason` in payload

**Phase 2:**
- `structural_summary` for Python: extracts function defs and class signatures
- `structural_summary` for Rust: extracts fn signatures, struct defs, impl blocks
- `truncate_at_boundary` never cuts mid-function (verify with known multi-line functions)
- Unknown file extension falls back to line-based truncation
- Malformed source file (parse error) falls back to line-based truncation
- Empty file returns empty string (no panic)
- Decomposer prompt with repo_path uses structural summary instead of line truncation

**Phase 3:**
- Cumulative dry-run: A merges clean, B conflicts on top of A -> only A returned
- Dry-run leaves repo state unchanged (reset to pre-merge SHA)
- First bundle conflicts -> empty result, tick transitions to Failed
- All bundles merge clean -> full list returned
- Bundles after a conflict are not tested (conservative exclusion)

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM ignores `requires_replacement` field | Medium | Low | Defaults to `false` (safe); prompt documents when to set each value |
| `unblock_dependents` promotes ExhaustedRetries work | High | High | Guard: only auto-unblock DependencyWait or legacy None-reason works |
| Tree-sitter grammar missing for target language | Medium | Low | Fallback to line-based truncation; start with Python/Rust/JS/TS which cover most targets |
| Tree-sitter build adds compile time | Low | Low | Grammar C compilation is cached by cargo; incremental builds unaffected |
| Dry-run merge leaves dirty git state | Low | High | Always `git merge --abort` after trial; verify SHA matches pre-merge; hard reset as safety net |
| Dry-run false negative (merge succeeds but build fails) | Certain | Low | Post-merge validation still runs. Dry-run catches git conflicts, not semantic failures. |

## Open Questions

- [ ] Should `requires_replacement` also apply to `OverridePhase` and `OverrideSpec`? Both have the same `{id, target_status, reason}` shape. Recommendation: yes.
- [ ] Should `blocked_reason` be included in the coordinator's status summary so it can make different decisions for exhausted-retries vs dependency-wait?
- [ ] Which tree-sitter grammars to include initially? Recommendation: Python, Rust, JavaScript, TypeScript - these cover the primary E2E targets.
- [ ] Should bundles that were untested (after the dry-run stops early) be excluded from the tick or included optimistically? Current design: excluded (conservative).

## References

- Parent design doc: `docs/design/2026-04-15-e2e-orchestration-fixes.md`
- OverrideWork struct: `src/agents/action.rs:164-168`
- validate_action_coherence: `src/agents/coordinator.rs:610-644`
- Coherence gate: `src/agents/coordinator/run.rs:307-325`
- Coordinator prompt: `resources/agents/coordinator.pmt:42-107`
- Work struct: `src/domain/work.rs:35-59`
- WorkStatus enum: `src/domain/work.rs:13-25`
- Work transition handler: `src/daemon/handlers/work.rs:505-606`
- unblock_dependents: `src/daemon/handlers/work.rs:328-376`
- Orphan recovery: `src/daemon/context.rs:473-476`
- Decomposer file injection: `src/daemon/handlers/decomposer.rs:400-500`
- Integrator merge: `src/agents/integrator.rs:1680-1740`
- Orchestration spine design: `docs/design/2026-02-25-orchestration-spine.md`
