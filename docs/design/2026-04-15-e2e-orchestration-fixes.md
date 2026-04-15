# Design Document: E2E Orchestration Bug Fixes

**Author:** Scott Idler / Claude
**Date:** 2026-04-15
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Four bugs discovered during a python-api E2E run cause state/git fractures, silent status misreporting, coordinator deadlocks, and destructive decomposition. This document covers the agreed-upon fixes between Claude and the Gemini Architect, ordered by urgency: coordinator action coherence enforcement (Bug 4), integrator transition-gate (Bug 2), OVERRIDE logging accuracy (Bug 3), and decomposer workspace awareness (Bug 1).

## Problem Statement

### Background

During an E2E run against a python-api target, the orchestrator declared GoalComplete with exit 0 despite the API missing most of its routes. Post-mortem analysis identified 6 bugs; Bug 5 (GoalComplete counting Abandoned as terminal success) is already fixed in loopr-v4, Bug 6 (stale container from prior run) was operator error. The remaining 4 are code defects requiring fixes.

### Problem

The orchestrator has four independent failure modes that compound:

1. **Coordinator action coherence is advisory-only.** `validate_action_coherence` detects when the coordinator LLM promises to create replacement works but doesn't include the `create_work` actions. The warning is logged and ignored. With Bug 5's fix now in loopr-v4 (Abandoned/Blocked leaves Phase Active), an incoherent coordinator that abandons work without replacing it produces a **permanent deadlock** - the Phase never completes and there's no path to GoalComplete.

2. **Integrator continues after transition failure.** When a bundle can't transition from Accepted to Integrating (e.g., advisory lock held by a competing work item), `integrator.rs` logs a warning and proceeds to git merge. The bundle ends up merged into the integration branch but stuck in Accepted status. The subsequent Accepted -> Merged transition is invalid (Integrating is the required intermediate), producing a CRITICAL error and a permanent state/git fracture.

3. **OVERRIDE logging reports requested status, not actual status.** When `attempt_count` hits `MAX_WORK_ATTEMPTS` (3), the work's effective status becomes Blocked instead of the requested Ready. But both the `tracing::warn!` and `DaemonEvent::transition_completed` broadcast `target_status` (Ready), not `effective_status` (Blocked). Operators, the TUI, and any event consumers see the wrong state.

4. **Decomposer is structurally blind to workspace state.** `build_decompose_prompt` receives only the parent document's markdown content. It never sees `git ls-files`, existing file contents, or the repo tree. When a Plan says "create a FastAPI app" and `main.py` already exists with a `/health` route, the LLM generates specs saying "create main.py from scratch" - destroying existing work.

### Goals

- **Bug 4:** Promote coherence warnings to hard failures that force the coordinator LLM to retry with correct actions
- **Bug 2:** Gate git merge on successful bundle transition to Integrating; exclude failed bundles from the tick without aborting the entire tick
- **Bug 3:** Fix both logging surfaces to report `effective_status` instead of `target_status`
- **Bug 1:** Inject workspace file tree (and targeted file contents) into the decomposer prompt so specs don't instruct destruction of existing code

### Non-Goals

- Rewriting the decomposer's multi-level architecture (single-level decomposition is intentional)
- Adding automatic rollback for failed integrator ticks (existing reset-to-pre-merge-SHA logic is sufficient)
- Changing `MAX_WORK_ATTEMPTS` threshold or the Blocked status semantics
- Fixing Bug 5 or Bug 6 (already resolved)

## Proposed Solution

Phases are ordered by urgency, not by bug number:

| Phase | Bug | Fix | Model |
|-------|-----|-----|-------|
| 1 | Bug 4 | Coordinator coherence enforcement | opus |
| 2 | Bug 2 | Integrator transition gate | sonnet |
| 3 | Bug 3 | OVERRIDE logging accuracy | sonnet |
| 4 | Bug 1 | Decomposer workspace awareness | opus |

### Phase 1: Coordinator Action Coherence Enforcement (Bug 4)

**Model:** opus

**Root cause:** `validate_action_coherence` in `src/agents/coordinator.rs:610-640` returns `Vec<String>` warnings. The call site in `src/agents/coordinator/run.rs:308-313` logs them via `self.ctx.warn()` and continues to execute the incoherent actions.

**Fix:**

When `coherence_warnings` is non-empty, do not execute the actions. Instead, feed the warnings back to the LLM as a retry prompt. This uses the existing self-correction pattern already present for JSON parse errors.

**Prerequisite: Harden the heuristic before promoting it to a gate.** The current `validate_action_coherence` (coordinator.rs:622-627) matches on keywords like "replacement", "creating", "replacing" in the override reason. This produces false positives when the coordinator deliberately abandons without replacement - e.g., reason: "no replacement is needed". Before this becomes a blocking gate, add a negation guard:

```rust
let negation = lower.contains("no replacement")
    || lower.contains("not replacing")
    || lower.contains("without replacement")
    || lower.contains("replacement is not");
if mentions_create && !has_create && !negation {
```

This is a minimal fix to the existing heuristic, not a rewrite. The heuristic still only fires on `target_status == "Abandoned"` actions, so the blast radius is narrow.

**Where:** `src/agents/coordinator/run.rs`, replace lines 308-313 (the current warn-and-continue block).

```rust
// Phase 2: Action coherence validation - reject when coordinator promises
// to create replacement works but doesn't emit the create_work action.
let coherence_warnings = super::validate_action_coherence(&actions, &prefix);
if !coherence_warnings.is_empty() {
    for warning in &coherence_warnings {
        self.ctx.warn(warning);
    }
    // Count toward Lifeguard parse-failure ceiling so it escalates after threshold
    if let Verdict::Escalate(reason) = guard.record_parse_failure() {
        return Ok(IterationOutcome::NeedHelp(format!(
            "lifeguard: repeated coherence failures: {}", reason
        )));
    }
    let feedback = format!(
        "Your actions are incoherent. Fix the following issues and resubmit all actions:\n{}",
        coherence_warnings.join("\n")
    );
    return Ok(IterationOutcome::Continue(feedback));
}
```

**How the retry works:** `IterationOutcome::Continue(feedback)` stores the feedback string in `self.previous_summary` (run.rs:74). The outer FSM loop calls `run_iteration()` again, which injects `previous_summary` into the LLM prompt via `.with_previous_summary()` (run.rs:244). The LLM sees its own coherence failure and can correct the actions.

**Retry ceiling and the reset_parse_failures interaction:** There is a subtle ordering issue. Currently `guard.reset_parse_failures()` fires at run.rs:296, immediately after JSON parsing succeeds but BEFORE the coherence check. If coherence fails, `record_parse_failure()` sets the counter to 1. But on the next iteration, JSON parsing succeeds again and `reset_parse_failures()` clears the counter back to 0 - so it never accumulates, making the Lifeguard escalation unreachable.

**Fix:** Move `guard.reset_parse_failures()` from its current location (run.rs:296) to after the coherence check block. If the coherence check returns early (via `Continue`), the reset never fires and the counter accumulates. If the coherence check passes, the reset fires as usual. This is a one-line move, not a new mechanism.

```rust
// BEFORE (current):
guard.reset_parse_failures();  // line 296 - fires immediately after parse
// ... coherence check at line 308 ...

// AFTER (fixed):
// ... coherence check returns early if incoherent (record_parse_failure already called) ...
guard.reset_parse_failures();  // moved here - only fires when both parse AND coherence pass
```

**Why this is most urgent:** Bug 5's fix (Abandoned/Blocked leaves Phase Active) created a hard dependency on the coordinator producing valid replacements. Without this fix, the coordinator can strand a Phase permanently by promising replacements it never creates.

### Phase 2: Integrator Transition Gate (Bug 2)

**Model:** sonnet

**Root cause:** `src/agents/integrator.rs:525-541` iterates `valid_bundle_ids`, attempts Accepted -> Integrating transition for each, logs failures, but doesn't remove failed bundles from the vec. Lines 547-548 then persist all `valid_bundle_ids` (including failed ones) as `tick.bundle_ids`. Line 624+ merges all their branches.

**Fix:**

Replace the for-loop with a filter that only retains successfully transitioned bundles:

```rust
// 8. Transition bundles: Accepted -> Integrating (gate: only merge what transitions)
let valid_bundle_ids: Vec<String> = valid_bundle_ids
    .into_iter()
    .filter(|bundle_id| {
        let resp = self.ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({
                "id": bundle_id,
                "target_status": "Integrating",
                "role": "integrator",
            }),
        );
        if resp.is_error() {
            self.ctx.warn(&format!(
                "excluding bundle {} from tick: failed to transition to Integrating: {:?}",
                bundle_id, resp.error
            ));
            false
        } else {
            true
        }
    })
    .collect();

// If no bundles survived the gate, transition tick to Failed and return early.
// The tick was already created (step 6) and sealed (step 7) before we got here,
// so we can't just return Ok(()) - that would leave a sealed tick with no bundles.
if valid_bundle_ids.is_empty() {
    self.ctx.info("no bundles successfully transitioned to Integrating; failing tick");
    let _ = self.ctx.bridge.request(
        "tick.transition",
        serde_json::json!({
            "id": tick_id,
            "target_status": "Failed",
            "role": "integrator",
        }),
    );
    return Ok(());
}
```

**What happens to excluded bundles:** They stay in Accepted. Nothing is deleted, rejected, or marked. The next integrator cycle picks them up. By then the advisory lock that blocked them may have cleared.

**What happens to the tick when all bundles are excluded:** The tick was already created and sealed before the bundle transition step. If all bundles fail, the tick is transitioned to Failed so it doesn't remain as a dangling sealed tick with no resolution.

**Observability: attempted_bundle_ids vs bundle_ids:** After the filter, `valid_bundle_ids` only contains successfully transitioned bundles. At line 548, both `tick.bundle_ids` and `tick.attempted_bundle_ids` are set to this filtered list. To preserve observability into which bundles were excluded and why, save the pre-filter list as `attempted_bundle_ids` before running the filter:

```rust
let attempted_bundle_ids = valid_bundle_ids.clone();
let valid_bundle_ids: Vec<String> = valid_bundle_ids.into_iter().filter(|bundle_id| { ... }).collect();
// ...
tick.bundle_ids = valid_bundle_ids.clone();
tick.attempted_bundle_ids = attempted_bundle_ids;
```

**Independence assumption:** This fix assumes bundles within a tick are structurally independent - merging a subset doesn't break the integration branch. This holds because each bundle corresponds to a single Work item operating on its own worktree branch. If Bundle B depends on Bundle A's interface, either (a) Bundle A already merged in a prior tick and the interface is on the integration branch, or (b) the merge of B alone produces a git conflict, which the existing merge-failure rollback at integrator.rs:632-695 handles (resets to pre-merge SHA, transitions tick to Failed, rejects all bundles). The partial-filter approach does not bypass this safety net.

**Why filter, not abort:** A single locked bundle shouldn't prevent other bundles from merging. The partial approach is safer and prevents cascading stalls.

### Phase 3: OVERRIDE Logging Accuracy (Bug 3)

**Model:** sonnet

**Root cause:** `src/daemon/handlers/work.rs` computes `effective_status` at lines 505-518 (may diverge from `target_status` when `attempt_count >= MAX_WORK_ATTEMPTS`). But two downstream surfaces still use `target_status`:

1. **Line 564-570:** `DaemonEvent::transition_completed` broadcasts `target_status.to_string()`
2. **Lines 572-579:** `tracing::warn!` logs `target_status` in the OVERRIDE message, and the `work.override_transition` event also uses `target_status`

**Fix:** Replace `target_status` with `effective_status` at all three locations:

```rust
// Line 564-570: DaemonEvent
let _ = event_tx.send(DaemonEvent::transition_completed(
    "work",
    &id,
    &from.to_string(),
    &effective_status.to_string(),  // was: target_status
    &role.to_string(),
));

// Lines 572-579: OVERRIDE warn + event
if is_override {
    tracing::warn!(
        "OVERRIDE: Work {} transitioned {:?} -> {:?} by Coordinator (reason: {})",
        id,
        from,
        effective_status,  // was: target_status
        override_reason
    );
    let _ = event_tx.send(DaemonEvent::new(
        "work.override_transition",
        serde_json::json!({
            "work_id": id,
            "from": format!("{:?}", from),
            "to": format!("{:?}", effective_status),  // was: target_status
            "reason": override_reason,
        }),
    ));
}
```

Additionally, when `effective_status` diverges from `target_status`, log the divergence explicitly:

```rust
if effective_status != target_status {
    tracing::warn!(
        "Work {} attempt_count={} reached max; effective status forced {:?} -> {:?}",
        id, wi.attempt_count, target_status, effective_status
    );
}
```

### Phase 4: Decomposer Workspace Awareness (Bug 1)

**Model:** opus

**Root cause:** `build_decompose_prompt` in `src/daemon/handlers/decomposer.rs:400-421` constructs the LLM prompt from: instructions (.pmt file), template (.md), count guidance, dependency pattern, and parent document content. It already accepts `repo_path: Option<&std::path::Path>` but doesn't use it for file discovery. The LLM has zero visibility into existing workspace files.

**Fix (three parts):**

**Part A: Inject file tree into decomposer prompt**

When `repo_path` is `Some`, run `git ls-files` and include it in the prompt between the template and parent document sections:

```rust
let workspace_context = if let Some(path) = repo_path {
    let output = std::process::Command::new("git")
        .args(["ls-files"])
        .current_dir(path)
        .output()?;
    let file_list = String::from_utf8_lossy(&output.stdout);
    format!(
        "\n## Existing Workspace Files\n\n\
         The target repository already contains these files. \
         Do NOT instruct 'create from scratch' for any existing file. \
         Default to additive changes unless the parent document explicitly requires replacement.\n\n\
         ```\n{}\n```\n",
        file_list.trim()
    )
} else {
    String::new()
};
```

Insert `{workspace_context}` into the prompt template between `## Template` and `## Parent Document`.

**Part B: Inject targeted file contents for Plan-referenced files**

When `repo_path` is `Some` and `target_kind` is `DocKind::Spec` (Plan -> Specs decomposition), parse the parent Plan for file paths that appear in its outputs or deliverables. For each referenced file that exists in the workspace, include its contents (capped at 200 lines per file, 5 files max). This runs inside the same `if let Some(path) = repo_path` block as Part A.

**Path sanitization (security-critical):** `std::path::Path::join` replaces the base entirely if the RHS is an absolute path. Since `extract_file_references` parses LLM-generated markdown, it could yield `/etc/passwd` or `../../.aws/credentials`. All extracted paths must be sanitized before joining:

```rust
/// Sanitize a file reference from LLM-generated content.
/// Strips leading `/`, rejects `..` components, ensures result stays under repo_path.
fn sanitize_file_ref(repo: &std::path::Path, raw: &str) -> Option<std::path::PathBuf> {
    let trimmed = raw.trim_start_matches('/');
    let path = std::path::Path::new(trimmed);
    // Reject any path containing ".." components
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return None;
    }
    let full = repo.join(path);
    // Final check: resolved path must start with repo
    if full.starts_with(repo) {
        Some(full)
    } else {
        None
    }
}
```

```rust
let file_content_section = if let Some(repo) = repo_path {
    if target_kind == DocKind::Spec {
        let referenced_files = extract_file_references(parent_content);
        let mut buf = String::new();
        for file_ref in referenced_files.iter().take(5) {
            let Some(full) = sanitize_file_ref(repo, &file_ref.to_string_lossy()) else { continue };
            if full.exists() {
                if let Ok(content) = std::fs::read_to_string(&full) {
                    let truncated: String = content.lines().take(200).collect::<Vec<_>>().join("\n");
                    buf.push_str(&format!(
                        "\n### {}\n```\n{}\n```\n",
                        file_ref.display(), truncated
                    ));
                }
            }
        }
        if !buf.is_empty() {
            format!("\n## Existing File Contents\n{}", buf)
        } else {
            String::new()
        }
    } else {
        String::new()
    }
} else {
    String::new()
};
```

**Why Spec-level only:** Phase and Work decompositions operate on more narrowly scoped parent documents. Spec decomposition is where the "create from scratch" instructions originate because the Plan describes the full project shape. Lower-level decompositions inherit the Spec's instructions and don't independently decide to create files.

**Part C: Anti-destruction instruction in .pmt files**

Add to all three decomposer prompt files (`resources/decompose/{spec,phase,work}/prompt.pmt`):

```
CRITICAL: The target repository may already contain files from prior work or initialization.
Never instruct "create from scratch" or "write from scratch" for any file that appears in the
Existing Workspace Files listing. Default to modifying, extending, or appending to existing files.
Only create new files when they genuinely don't exist yet.
```

## Alternatives Considered

### Alternative 1: Abort entire tick on transition failure (Bug 2)
- **Description:** Return early from `run_cycle()` when any bundle fails to transition
- **Pros:** Simpler code; guarantees no state/git fracture
- **Cons:** A single locked bundle blocks all other bundles in the tick. Multi-bundle ticks become fragile. Cascading stalls likely.
- **Why not chosen:** The partial-filter approach is strictly better. Failed bundles stay safe in Accepted; successful bundles proceed.

### Alternative 2: NeedHelp instead of retry for coherence (Bug 4)
- **Description:** Return `IterationOutcome::NeedHelp` immediately when coherence fails
- **Pros:** Stops the coordinator immediately; surfaces the problem
- **Cons:** The LLM might produce correct actions on a second attempt. NeedHelp is a terminal state that requires human intervention. Premature.
- **Why not chosen:** Self-correction via retry is cheaper and already proven for JSON parse errors. NeedHelp is the fallback when retries are exhausted.

### Alternative 3: Full workspace snapshot for decomposer (Bug 1)
- **Description:** Include full file contents for all workspace files
- **Pros:** Maximum context for the LLM
- **Cons:** Blows up prompt size for large repos. Expensive. Most files are irrelevant to any given decomposition.
- **Why not chosen:** File tree + targeted contents for referenced files is a practical middle ground. Covers the specific failure mode without the cost.

## Technical Considerations

### Dependencies

- No new crate dependencies required
- Bug 1 uses `std::process::Command` for `git ls-files` (already used elsewhere in the codebase)

### Performance

- Bug 4: One additional LLM round-trip on incoherent actions (rare case)
- Bug 2: Negligible - filter replaces a for-loop
- Bug 3: Zero cost - same logging, different variable
- Bug 1: One `git ls-files` call per decomposition (~ms). File reads capped at 5 files x 200 lines.

### Testing Strategy

- **Bug 4:** Unit test: mock coordinator actions with override_work mentioning "replacement" but no create_work -> assert IterationOutcome::Continue with feedback message. Additional test: override_work with reason "no replacement is needed" and no create_work -> assert NO coherence warning (negation guard).
- **Bug 2:** Unit test: mock bridge where one bundle transition fails -> assert it's excluded from tick.bundle_ids and git merge proceeds with remaining bundles
- **Bug 3:** Unit test: set attempt_count to MAX_WORK_ATTEMPTS-1, trigger Ready transition -> assert DaemonEvent and log contain "Blocked", not "Ready"
- **Bug 1:** Unit test: build_decompose_prompt with repo_path pointing to a temp dir with known files -> assert prompt contains "Existing Workspace Files" section. Additional tests: `sanitize_file_ref` rejects `/etc/passwd`, rejects `../../.env`, accepts `src/main.py`.

### E2E Validation

Re-run the python-api E2E target after all fixes. The specific assertions:
- Coordinator retries when it promises replacements without creating them
- Bundles that can't transition to Integrating are excluded from the merge, not silently merged
- Logs accurately report Blocked when attempt_count triggers it
- Decomposer specs say "modify main.py" not "create main.py from scratch" when main.py exists

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coherence retry loop (Bug 4): LLM fails to self-correct | Medium | Medium | Coherence failures count toward Lifeguard parse-failure threshold; escalates to NeedHelp after 3 failures |
| Filter empties all bundles (Bug 2): tick becomes a no-op | Low | Low | Early return with info log; bundles retry next cycle |
| git ls-files slow on large repos (Bug 1) | Low | Low | Output is small (file paths only); runs once per decomposition |
| File content injection exceeds prompt limits (Bug 1) | Medium | Medium | Capped at 5 files x 200 lines; future: smarter selection heuristic |
| effective_status change breaks event consumers (Bug 3) | Low | Low | Consumers should already handle Blocked; this fixes a lie, not a contract |
| git ls-files misses untracked files (Bug 1) | Low | Medium | If the init commit hasn't been made, new files won't appear in the listing. Acceptable: decomposition runs after init, and the anti-destruction .pmt instruction is defense-in-depth |
| Path traversal via LLM-generated file refs (Bug 1) | Medium | High | `sanitize_file_ref` strips leading `/`, rejects `..`, and validates resolved path stays under repo_path |
| Coherence false positive on deliberate abandonment (Bug 4) | Medium | High | Negation guard added to heuristic before it becomes a blocking gate; "no replacement" phrases bypass the check |

## Open Questions

- [ ] Should the coherence retry (Bug 4) include the original actions in the retry prompt so the LLM can amend them, or should it start fresh?
- [ ] For Bug 1, `extract_file_references` is new code that needs to be written. Should it be a regex heuristic (match paths ending in common extensions like `.py`, `.rs`, `.yml`) or should it parse the Plan's structured sections (e.g., the "Deliverables" or "Outputs" headings)?
- [ ] Is 200 lines per file and 5 files max the right cap for Bug 1's content injection, or should this be configurable?

## References

- E2E run logs: `docs/e2e/` directory
- Integrator code: `src/agents/integrator.rs:525-695`
- Work FSM handler: `src/daemon/handlers/work.rs:505-579`
- Coordinator coherence: `src/agents/coordinator.rs:610-640`, `src/agents/coordinator/run.rs:308-313`
- Decomposer prompt: `src/daemon/handlers/decomposer.rs:400-421`
- Design doc: `docs/design/2026-02-25-orchestration-spine.md` (integrator architecture)
- Design doc: `docs/design/2026-02-26-multi-level-rwl.md` (coordinator/decomposer roles)
