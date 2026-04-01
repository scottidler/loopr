# Design Document: No-Op Bundle Pathway

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 4/4

## Summary

When an Implementer discovers that its assigned Work is already complete (e.g., a Phase 1 agent "over-delivered" and Phase 2 has nothing to do), it has no clean way to signal this. The current system demands a git diff, and the Reviewer rejects empty bundles. This document adds a `noop_reason` field to the Bundle pipeline, enabling Implementers to declare "nothing to change" while preserving the full Reviewer/Integrator verification chain.

## Problem Statement

### Background

The Loopr orchestration pipeline assumes every Bundle contains a non-empty git diff. The Implementer commits code to a worktree branch (`agent/{work_id}`), proposes a Bundle, the Reviewer evaluates the diff against acceptance criteria, and the Integrator merges the branch into main.

This assumption breaks when an earlier phase over-delivers. In a real E2E run, the Phase 1 Implementer was told "use basic HTML, Tailwind comes in Phase 2" but styled the app with Tailwind anyway. The Reviewer approved it (non-blocking). When Phase 2 launched, its Implementer found the work already done, submitted an empty-diff bundle, and the Reviewer rejected it: "no actual code diff or file contents were included."

### Problem

The pipeline enters a deadlock:
1. The Implementer correctly identifies that no code changes are needed
2. It proposes a bundle with no diff (the branch has no new commits)
3. The Reviewer's prompt says "evaluate the code changes" - but there are none
4. The Reviewer rejects the bundle for having no proof of work
5. The Coordinator sees a rejected bundle and reassigns the work
6. A new Implementer arrives at the same conclusion - infinite loop

**Root cause:** The pipeline has no concept of "verified no-op." An Implementer can only succeed by producing a diff. There is no mechanism to declare "the acceptance criteria are already satisfied by the current state of the codebase."

### Goals

- Allow Implementers to propose a no-op bundle with an explicit reason
- Ensure the Reviewer still verifies the claim by reading the codebase (not trusting the Implementer blindly)
- Ensure the Integrator skips the merge but still runs validation commands
- No new FSM states, no new IPC methods, no new agent types

### Non-Goals

- Preventing over-delivery by Phase 1 (separate concern - prompt hardening)
- Auto-detecting no-op situations (the Implementer must explicitly declare it)
- Changing the FSM transition rules for Bundles or Work

## Proposed Solution

### Overview

Add `noop_reason: Option<String>` to `AgentAction::ProposeBundle`, the `Bundle` domain model, and propagate it through the executor, Reviewer context, and Integrator merge path. When set, the executor skips the auto-commit and uses an empty `branch_name`. The Reviewer receives a modified prompt directing it to verify the codebase state against acceptance criteria. The Integrator's existing empty-branch filter (`!b.branch_name.is_empty()`) already skips the merge, but validation commands still run.

### Implementation

#### Phase 1: Data Model + Executor Plumbing

**File: `src/agents/mod.rs` (AgentAction enum, line 445)**

Add `noop_reason` to `ProposeBundle`:

```rust
ProposeBundle {
    #[serde(default, alias = "summary")]
    description: String,
    #[serde(default, deserialize_with = "string_or_vec")]
    claims: Vec<String>,
    #[serde(default)]
    noop_reason: Option<String>,
},
```

**File: `src/domain/bundle.rs` (Bundle struct, line 171)**

Add `noop_reason` field:

```rust
#[serde(default)]
pub noop_reason: Option<String>,
```

**File: `src/daemon/handlers/bundle.rs` (handle_bundle_create, line 30)**

- Parse `noop_reason` from request params
- When `noop_reason.is_some()`, skip the `branch_name` required check (lines 72-77). Instead, default `branch_name` to empty string.
- Set `bundle.noop_reason` from the parsed value

**File: `src/agents/executor.rs` (ProposeBundle handler, line 759)**

When `noop_reason.is_some()`:
- Skip the auto-commit block entirely (no `git add -A`, no `git commit`)
- Set `branch_name` to empty string instead of `format!("agent/{}", wi_id)`
- Pass `noop_reason` to the `bundle.create` RPC params

#### Phase 2: Reviewer Awareness

**File: `src/agents/context.rs` (load_bundle_hierarchy, line 351)**

Two changes in this file:

**Change 1:** In `load_bundle_hierarchy()` (line 351), add `noop_reason` to the destructured fields and store it on `self`:

```rust
// In the destructuring block (line 353-364), add:
let noop_reason = bundle.noop_reason.clone();
// After line 366, store it:
self.bundle_noop_reason = noop_reason;
```

Add `bundle_noop_reason: Option<String>` field to the `ContextBuilder` struct.

**Conditional diff skip:** When `noop_reason.is_some()`, skip the `git diff HEAD agent/{work_id}` command entirely (lines 369-384). There is no branch to diff. Instead, read the parent Work's `resource_tags` files from the repo root and store them as `self.noop_file_contents: Option<Vec<(String, String)>>` (path, content pairs).

**Change 2:** In the bundle section builder (line 526), when `bundle_noop_reason.is_some()`, replace the "Code Changes" diff section with a no-op directive:

```
**NO-OP BUNDLE** - The Implementer made no code changes.

**Implementer's claim:** {noop_reason}

**Your task:** Do NOT look for a diff. Instead, use the file contents
provided below and verify the codebase's CURRENT STATE against every
acceptance criterion. If the criteria are already satisfied, approve.
If not, reject with specifics about what is missing.
```

Then inject the content of the Work's `resource_tags` files (read from the repo, not from a diff) so the Reviewer has actual code to verify against. Note: `touched_paths` will be empty for noop bundles since no files were modified - `resource_tags` from the parent Work define the relevant scope.

Additionally, the context builder must skip the `git diff HEAD agent/{work_id}` call when `noop_reason` is set, since the branch may not exist or may have no commits. Instead of a diff, read the current file contents from main.

**File: `prompts/reviewer.pmt`**

Add a section documenting no-op bundle behavior so the LLM knows what to expect:

```
## No-Op Bundles

If the bundle section says "NO-OP BUNDLE", the Implementer claims the Work is
already complete without code changes. You must:
1. Read the provided file contents carefully
2. Verify EVERY acceptance criterion against the current code
3. Approve if all criteria are met; reject if any are not
4. Do NOT reject solely because there is no diff
```

#### Phase 3: Implementer Prompt Update

**File: `prompts/implementer.pmt`**

Update the `propose_bundle` action description and add guidance:

```
6. `propose_bundle` - Submit your work as a Bundle for review.
   If the Work is already complete (acceptance criteria already satisfied
   by the current codebase), use `"noop_reason": "explanation"` instead
   of making unnecessary changes. You must still verify via tools first.
```

Add to the Workflow section:

```
### No-Op Detection

If after reading the code you determine ALL acceptance criteria are already
satisfied by the current state:
1. Run the verification tools (test, clippy, fmt) to confirm
2. If tools pass, propose a no-op bundle (skip `commit` - there are no changes):
   [{"action": "propose_bundle",
     "description": "Work already complete",
     "claims": ["criteria X satisfied", "criteria Y satisfied"],
     "noop_reason": "Phase 1 implementer already added Tailwind classes..."},
    {"action": "done", "summary": "Work already complete - proposed noop bundle"}]
3. Do NOT use `commit` before a noop `propose_bundle` - there is nothing to commit
4. Do NOT make cosmetic changes just to produce a diff
```

#### Phase 4: Integrator Logging

**File: `src/agents/integrator.rs` (merge phase, around line 509)**

The existing filter `!b.branch_name.is_empty()` already skips noop bundles during merge. Add explicit logging:

```rust
// Before building the branches list (uses a separate pass over the collected bundle IDs)
let noop_count = tick_bundle_ids.iter()
    .filter(|id| bundles.get(*id).map_or(false, |b| b.noop_reason.is_some()))
    .count();
if noop_count > 0 {
    info!("Skipping merge for {} noop bundle(s)", noop_count);
}
```

Note: The noop count must be computed from a collected `Vec<String>` of bundle IDs, not from the `valid_bundle_ids` iterator, which is consumed when building the branch list.

Validation commands still run (they operate on the repo, not the bundle branch). Tick publishing and Work transitions proceed normally.

### Data Flow

```
Implementer LLM
  -> ProposeBundle { description, claims, noop_reason: Some("...") }
    -> executor.rs: skip auto-commit, set branch_name = ""
    -> bundle.create RPC: allow empty branch_name when noop_reason is set
    -> Bundle record created with noop_reason persisted
      -> Coordinator triages bundle (normal flow)
        -> Reviewer: context builder injects noop directive + file contents
        -> Reviewer: verifies codebase state against acceptance criteria
        -> Reviewer: approve/reject (normal verdict)
          -> Integrator: filter skips empty branch_name (no merge)
          -> Integrator: runs validation_commands against main
          -> Integrator: publishes Tick, transitions Bundle -> Merged, Work -> Integrated
```

### Testing Strategy

1. **Unit test - noop bundle creation:** Create a Bundle with `noop_reason` set, verify `branch_name` is empty, verify record persists correctly
2. **Unit test - bundle.create allows empty branch_name with noop_reason:** Verify the handler accepts empty `branch_name` when `noop_reason` is provided, and still rejects empty `branch_name` when `noop_reason` is absent
3. **Unit test - executor skips auto-commit for noop:** Mock the executor path with `noop_reason` set, verify no git commands are executed
4. **Unit test - context builder injects noop directive:** Build Reviewer context for a noop bundle, verify the output contains "NO-OP BUNDLE" and does not contain "Code Changes"
5. **Unit test - Integrator skips merge for noop:** Create a Tick with a noop bundle, verify merge is skipped but validation commands run
6. **Unit test - noop serde round-trip:** Verify `Bundle` with `noop_reason` serializes/deserializes correctly (backward compat: old records without the field deserialize with `None`)
7. **Unit test - mixed tick with noop + normal bundle:** Create a Tick containing one noop bundle and one normal bundle, verify only the normal bundle's branch is merged while the noop is skipped, and both transition to Merged

## Alternatives Considered

### Alternative 1: Direct-to-Done Override

- **Description:** Give the Implementer a `mark_already_done` tool that bypasses the Reviewer/Integrator entirely, transitioning Work from InProgress to Done.
- **Pros:** Simple, zero pipeline changes.
- **Cons:** Breaks the trust model. The Implementer (lowest-trust agent) self-certifies success without verification. A hallucinating LLM could mark everything Done.
- **Why not chosen:** The Reviewer must remain the gatekeeper. The whole point of the pipeline is that claims are verified.

### Alternative 2: Separate noop_bundle Tool

- **Description:** Add a new `submit_noop_bundle` action distinct from `propose_bundle`.
- **Pros:** Explicit separation of concerns.
- **Cons:** Adds a new action to the LLM's context window, complicates the parser, and duplicates logic (both tools create a Bundle record). The LLM must choose between two similar tools, increasing confusion.
- **Why not chosen:** Extending the existing `propose_bundle` is simpler and leverages the same pipeline. One tool, one optional field.

### Alternative 3: Reviewer Prompt-Only Fix

- **Description:** Teach the Reviewer to handle empty diffs without any data model changes. If no diff is present, assume the Implementer claims completion and verify the codebase.
- **Pros:** Zero code changes.
- **Cons:** Fragile. The Reviewer would need to infer "no diff means no-op" rather than receiving an explicit signal. The executor would still try to auto-commit (and fail silently). The bundle.create handler would still reject empty branch_names.
- **Why not chosen:** Implicit behavior is unreliable. An explicit `noop_reason` field gives every component a clear signal to adjust its behavior.

## Technical Considerations

### Dependencies

None. Uses existing Bundle struct, executor, context builder, and Integrator infrastructure.

### Performance

Zero impact. The noop path is strictly less work than the normal path (no git operations, no diff computation).

### Backward Compatibility

The `noop_reason` field is `Option<String>` with `#[serde(default)]`. Existing Bundle records without this field deserialize to `None`. No migration needed. Old Implementer agents that don't know about `noop_reason` continue to work unchanged.

### Testing Strategy

See Implementation section above for 6 unit tests covering each touch point.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Implementer abuses noop to skip actual work | Low | Medium | Reviewer still verifies the codebase against acceptance criteria. A false noop claim gets rejected. |
| Reviewer fails to verify codebase state for noop | Low | Medium | The noop directive in the prompt is explicit and strongly worded. The Reviewer also receives file contents, not just the claim. |
| Old bundles without noop_reason cause deserialization errors | Very Low | Low | `#[serde(default)]` ensures backward compatibility. Field defaults to `None`. |
| Implementer makes unnecessary cosmetic changes instead of using noop | Low | Low | Prompt guidance explicitly says "Do NOT make cosmetic changes just to produce a diff." Worst case, the change goes through the normal pipeline - no harm. |

## Open Questions

- [ ] Should the Coordinator's `build_state_summary()` surface noop bundles differently? (e.g., "Work X has a noop bundle pending review" vs. the generic "bundle in Triaged")

## Resolved Questions

- **Should `touched_paths` for a noop bundle list files the Implementer read?** Yes. The context builder uses `resource_tags` from the parent Work to determine which files to show the Reviewer. But the Implementer should also populate `touched_paths` with the specific files it verified, giving the Reviewer a focused subset. The context builder should prefer `touched_paths` when non-empty, falling back to `resource_tags`.

## References

- `src/agents/mod.rs:445-450` - AgentAction::ProposeBundle definition
- `src/domain/bundle.rs:169-212` - Bundle struct and constructor
- `src/agents/executor.rs:759-829` - ProposeBundle executor handler
- `src/daemon/handlers/bundle.rs:30-187` - bundle.create handler
- `src/agents/context.rs:351-387` - Reviewer context builder (diff injection)
- `src/agents/context.rs:526-559` - Bundle section in Reviewer prompt
- `prompts/implementer.pmt` - Implementer prompt template
- `prompts/reviewer.pmt` - Reviewer prompt template
- `src/agents/integrator.rs:509-577` - Integrator merge phase (branch filter at line 515)
- `docs/design/2026-04-01-merged-bundle-override-guard.md` - Related: guard against override race with merged bundles
