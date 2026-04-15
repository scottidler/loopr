# Design Document: Reviewer Hallucination Fixes

**Author:** Scott A. Idler
**Date:** 2026-04-15
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

A python-api E2E run revealed three compounding failures that trapped agents in an unrecoverable death loop: the reviewer never reads dependency files before rejecting on interface grounds, the learnings system institutionalizes hallucinations as authoritative facts, and there is no mechanism for an implementer to dispute a review verdict. This design fixes all three.

## Problem Statement

### Background

During a python-api E2E run, `wk-mvhyo` (CRUD endpoints in `main.py`) was abandoned after 3 rejections. The implementer correctly read `database.py` from the integration branch and used `**fields` to unpack a dict into the keyword-arg signature of `update_bookmark(db_path, bookmark_id, title=None, url=None, tags=None)`. The reviewer rejected it three times, citing "the database.py interface contract specifies `update_bookmark(db_path, id, fields: dict)` — unpacking will cause a TypeError." That contract was the spec text from decomposition time; the actual merged code had a different (equally valid) signature. The reviewer never read the file.

The failure was not one bad review. It was three interlocking failures that made self-correction impossible.

### Problem 1: Reviewer's cross-boundary signature extraction silently yields nothing when the bundle adds new imports

The reviewer already has a mechanism to read dependency files: `extract_referenced_signatures` in `src/agents/reviewer.rs:88-202`. It parses import statements from touched files, resolves them to paths, and extracts function signatures. This runs at `reviewer.rs:251-262`.

The critical bug: it reads touched files from `repo_path` — the main repo's current state — not from the bundle's worktree. When the bundle *adds* new import statements (e.g. the implementer adds `from database import update_bookmark` to `main.py`), those imports do not exist in the main repo's `main.py` yet. The main repo still has the pre-bundle version. Import parsing finds nothing, the sig section is empty, and the learnings dominate unchallenged.

When `database.py` was merged as part of an earlier work item, its actual signature became ground truth. The reviewer for `wk-mvhyo` had the machinery to read it — but the inputs to that machinery were wrong. The fix is not to add new functionality; it is to feed `extract_referenced_signatures` the bundle's version of touched files rather than the main repo's version.

### Problem 2: Learnings system institutionalizes hallucinations

When the first reviewer rejected `bd-h2a2z`, the framework persisted the hallucinated reasoning as global learnings (e.g. `ln-zb3wu`, `ln-b7o3d` in `.taskstore/learnings.jsonl`). Subsequent reviewers received those learnings in their context at high weight — the Reviewer role receives a 2000-token learnings budget with a 0.3 confidence minimum threshold (`src/agents/context.rs:803-825`). The implementer wrote the correct reasoning in the bundle claims ("update_bookmark called with \*\*fields matching actual database.py keyword arg signature"), but the reviewer's context treated the learning as a system fact and overrode the implementer's explicit claim.

The `Learning` struct has `contradictions: u32` and `confidence: f32` fields (`src/domain/learning.rs:44-69`), but there is no mechanism to:
- Require a code citation for structural claims ("function X has signature Y")
- Mark a learning tentative pending verification
- Increment contradictions when an implementer explicitly disputes it

### Problem 3: No dispute mechanism

When `wk-mvhyo` was reset to Ready after the first rejection, the coordinator embedded the wrong rejection reason in the override_work reason string (`src/agents/coordinator.rs:196-207`). The next implementer received this as authoritative context. After 3 rejections for the same reason, the work was abandoned — the system has no path between "retry with the same conditions" and "abandon."

The implementer tried to push back: its bundle claims explicitly addressed the previous rejection. There is no code that detects this contradiction and routes to arbitration instead of the standard review queue.

### Goals

- Reviewer context includes actual content of cross-boundary dependency files before it is allowed to reject on interface/signature grounds
- Learnings that make structural code claims require a code citation; uncited structural learnings are rendered as tentative and cannot override an implementer's contrary claim with a citation
- When an implementer's bundle claims directly address a prior rejection reason for the same work item, the bundle is routed to an arbitration path rather than the normal review queue

### Non-Goals

- Static type checking integration (mypy/pyright) — would not have fixed this failure; the `**fields` call is correct Python for the actual keyword-arg signature
- Changing the reviewer from single-iteration to multi-iteration — arbitration is a separate concern
- Fixing the decomposer's interface contract generation — contracts drifting from implementation is expected and handled downstream by this design
- Changing the overall bundle FSM topology

## Proposed Solution

### Overview

Three independent changes, each shippable alone:

1. **Phase 1 — Fix sig extraction source**: `extract_referenced_signatures` already exists and reads cross-boundary dep files. Fix it to parse imports from the bundle's git HEAD commit instead of the main repo's pre-bundle file version.
2. **Phase 2 — Learning quality gate**: Add `code_citation` and `tentative` fields to `Learning`. Structural claims without citations are tentative and cannot override implementer claims. Raise the confidence threshold for reviewer context.
3. **Phase 3 — Dispute detection and arbitration**: When a new bundle for a work item that has a prior rejection explicitly addresses that rejection in its claims, route it to a dedicated arbitration path. The arbitrator is a reviewer variant whose context pre-injects the disputed file rather than relying on the poisoned learnings.

### Architecture

#### Phase 1: Fix cross-boundary signature extraction to read from the bundle's git commit

**Where:** `src/agents/reviewer.rs:88-202` (`extract_referenced_signatures`) and callsite at `reviewer.rs:251-262`.

The function already exists and already reads cross-boundary dep files. The bug: it reads touched files from `repo_path` (the main project directory, pre-bundle state). When a bundle adds new import statements, those imports aren't in the main repo's version of the file yet — so import parsing yields nothing and the sig section is empty.

**Fix:** Parse import statements from the bundle's version of each touched file (via `git show {bundle.head_commit}:{file}` in the repo), not from the main repo's pre-bundle version. Dep modules are still resolved against `repo_path` (which has all previously-merged work on disk).

The worktree is cleaned up before the reviewer runs, so it cannot be used as a source. The bundle's `head_commit` field persists and is always available.

Updated function signature:
```rust
pub fn extract_referenced_signatures(
    repo_path: &Path,      // resolve dep files here (main repo, has merged work)
    head_commit: &str,     // read touched files from this commit via git show (NEW)
    work_files: &[String],
) -> String
```

Inside the function, replace:
```rust
let Ok(content) = std::fs::read_to_string(&file_path) else { continue; };
```
with:
```rust
let Ok(output) = std::process::Command::new("git")
    .args(["show", &format!("{}:{}", head_commit, work_file)])
    .current_dir(repo_path)
    .output() else { continue; };
let Ok(content) = String::from_utf8(output.stdout) else { continue; };
```

Callsite change in `reviewer.rs:251-262`:
```rust
let sig_section = {
    let (touched, head_commit) = self.ctx.stores
        .read_bundles()
        .ok()
        .and_then(|b| b.get(self.bundle_id.as_str()).map(|b| (b.paths.clone(), b.head_commit.clone())))
        .unwrap_or_default();
    let repo_path = &self.ctx.stores.config.project.repo_path;
    extract_referenced_signatures(repo_path, &head_commit, &touched)
};
```

**Section annotation:** Update the existing `## Referenced Signatures` header to make its authority explicit:
```
## Referenced Signatures (Cross-Boundary Dependencies)

These signatures were read from files already merged into the integration branch.
They are ground truth. If they differ from the work item's interface contract,
trust the code here — the spec may be stale. Do NOT reject on that basis.
```

**Token ceiling:** The sig section is currently appended without a budget. Add a `MAX_SIG_TOKENS: usize = 3000` ceiling constant in `reviewer.rs` and truncate the `all_sigs` vec before rendering if the estimated token count exceeds it.

#### Phase 2: Learning quality gate

**Changes to `Learning` struct** (`src/domain/learning.rs`):

```rust
pub struct Learning {
    // ... existing fields ...
    pub code_citation: Option<CodeCitation>,  // new
    pub tentative: bool,                       // new, default false
}

pub struct CodeCitation {
    pub file_path: String,
    pub line_number: u32,
    pub excerpt: String,  // the relevant line(s)
}
```

**Promotion rule:** A learning is `tentative = true` if:
- Its content contains any of the specific structural claim markers: "signature", "interface contract", "function contract", "has signature", "takes a"
- AND `code_citation` is None

Intentionally narrow: "type", "parameter", "argument", and "returns" alone are too broad and would mark legitimate non-code learnings as tentative. The markers above are specific to claims about function/API shape.

**Rendering in context** (`src/agents/context.rs:803-825`):
- Tentative learnings are rendered with a prefix: `[TENTATIVE - unverified structural claim]`
- Tentative learnings cannot be used to override an implementer's bundle claim that cites actual code (enforced by prompt language, not code)

**Confidence threshold for Reviewer:** Raise from 0.3 to 0.6. A low-confidence tentative learning should not appear in reviewer context at all.

**Contradiction increment:** When a bundle is proposed for a work item that has a prior rejection, and the new bundle's claims explicitly address that rejection, increment `contradictions` on the relevant learning immediately (at bundle proposal time, not at reviewer verdict time). This lowers the learning's confidence before the next reviewer even sees it.

#### Phase 3: Dispute detection and arbitration

**Detection:** In the bundle proposal handler (`src/daemon/handlers/bundle.rs` or equivalent), when a bundle is created for a work item that already has at least one `Rejected` bundle:

1. Fetch the `verification` field from the most recent rejected bundle (the rejection reason)
2. Check if the new bundle's `claims` contain semantic overlap with the rejection reason
3. Detection heuristic: if any claim contains words from the rejection reason's key terms (function name, parameter name, the word "signature", "interface", "contract"), flag as `disputed = true` on the new bundle

Add `disputed: bool` to the `Bundle` struct (default false, non-breaking).

**Routing:** The worker pool's review assignment checks `bundle.disputed`. If `true`, skip the normal review queue and route to the arbitration path.

**Arbitration path:**

Reuse `Reviewer` with a different prompt resource (`resources/agents/arbitrator.pmt`). No new `AgentType` needed — the same reviewer infrastructure handles it, just with different context assembly rules:

1. The previous rejection reason and the implementer's disputed claim are placed at the top, side by side, under a `## Dispute` heading
2. The cross-boundary file content is pre-injected (Phase 1 mechanism, always populated for arbitration) — no tool needed
3. All learnings scoped to this work item are excluded from the arbitrator's context; only promoted global policies are included
4. The arbitrator's prompt explicitly states: "A prior reviewer rejected this bundle. The implementer disputes that verdict. Your job is to adjudicate based on the actual code, not the spec or prior review feedback."

**Arbitrator output schema:** Extend the existing `ReviewResult` struct with an optional `citation` field:

```rust
pub struct ReviewResult {
    pub verdict: Verdict,   // Approve | Reject (same as reviewer)
    pub summary: String,
    pub issues: Vec<String>,
    pub citation: Option<String>,  // new: "path/to/file.py:42: excerpt" for rejections
}
```

The arbitrator prompt requires: if verdict is `Reject`, `citation` must be non-empty and name a file. The framework validates this at parse time — if `verdict == Reject && citation.is_none_or_empty()`, treat as a parse failure and requeue (max 1 retry, same as the existing `max_requeries` mechanism).

**Arbitrator verdict:**
- Approve: bundle proceeds normally; Phase 2's contradiction increment fires against the associated learnings for this work item
- Reject with citation: rejection stands; learning reinforcements increment; coordinator override_work proceeds normally
- Reject without citation: invalid — arbitrator re-queued (max 1 retry before abandoning the bundle)

**Coordinator change:** The coordinator's rejected-bundle detection (`src/agents/coordinator.rs:165-210`) should NOT emit `override_work` for disputed bundles — the arbitrator handles the resolution. Add a guard: skip the override_work action if `bundle.disputed == true`.

### Data Model

**Learning additions:**

```rust
// src/domain/learning.rs
pub struct CodeCitation {
    pub file_path: String,
    pub line_number: u32,
    pub excerpt: String,
}

pub struct Learning {
    // existing fields unchanged ...
    #[serde(default)]
    pub code_citation: Option<CodeCitation>,
    #[serde(default)]
    pub tentative: bool,
}
```

**Bundle addition:**

```rust
// src/domain/bundle.rs
pub struct Bundle {
    // existing fields unchanged ...
    #[serde(default)]
    pub disputed: bool,
}
```

Both additions use `#[serde(default)]` — fully backward-compatible with existing JSONL records.

**Retroactive migration for Phase 2:** Existing learnings in `.taskstore/learnings.jsonl` that pre-date this change will deserialize with `tentative: false` (the serde default). At daemon startup, a one-time migration pass should retroactively set `tentative: true` on any learning that matches the structural keyword markers and has no `code_citation`. This ensures poisoned learnings already in the store are correctly labelled before the next reviewer sees them. The migration runs as part of the `Stores::open()` initialization path and is idempotent.

### Implementation Plan

#### Phase 1: Fix cross-boundary signature extraction to read from the bundle's git commit
**Model:** sonnet

- Replace `source_path: &Path` parameter in `extract_referenced_signatures` with `head_commit: &str`; read touched files via `git show {head_commit}:{file}` instead of `std::fs::read_to_string`; keep dep-module resolution as `std::fs::read_to_string` against `repo_path` (those files are already on disk from prior merges); if `head_commit` is empty (noop bundles have no commit), fall back to the existing `repo_path` read behavior
- Update the callsite in `reviewer.rs:251-262` to extract `bundle.head_commit` alongside `bundle.paths` and pass it to the function
- Add `MAX_SIG_TOKENS: usize = 3000` ceiling; estimate tokens on `all_sigs.join("\n")` and truncate the vec if exceeded
- Update the `## Referenced Signatures` section header to explicitly label these as ground truth and instruct the reviewer not to reject on spec-vs-code discrepancies
- Unit tests: construct a fake git repo with a committed `main.py` (containing `from database import update_bookmark`) and a `database.py` with keyword-arg signature; verify the output contains the keyword-arg signature; verify an older commit of `main.py` (without the import) yields empty output; verify token ceiling truncation

#### Phase 2: Learning quality gate
**Model:** opus

- Add `CodeCitation` struct and `code_citation: Option<CodeCitation>` + `tentative: bool` to `Learning`
- Add serde default so existing records deserialize cleanly
- Implement `tentative` auto-promotion: at creation time, if content contains structural keywords and `code_citation` is None, set `tentative = true`
- Raise Reviewer confidence threshold from 0.3 to 0.6 in `select_learnings()` (`context.rs`)
- Update context rendering to prefix tentative learnings with `[TENTATIVE]`
- In bundle proposal handler: when prior rejected bundle exists, check for claim/rejection semantic overlap; if match, increment `contradictions` on the associated learning(s)
- Update reviewer prompt text to make explicit: tentative learnings are hypotheses, not facts; implementer claims citing actual code outweigh tentative learnings
- Unit tests: structural-claim learning auto-marked tentative; non-structural learning not marked; contradiction increment on dispute; tentative prefix in rendered context; high-confidence learning appears, low-confidence tentative learning excluded for reviewer

#### Phase 3: Dispute detection and arbitration
**Model:** opus

- Add `disputed: bool` to `Bundle` struct with serde default
- In bundle proposal handler: query prior rejected bundles for the same `work_id`; if any exist, check claim/rejection keyword overlap (function name, "signature", "interface contract", "function contract", "has signature"); if overlap found, set `disputed = true`
- Add `citation: Option<String>` to `ReviewResult` struct; update parse logic to require non-empty citation when verdict is Reject for arbitration responses
- Create `resources/agents/arbitrator.pmt` with: dispute context at top (prior rejection vs. implementer claim side by side), explicit instruction to adjudicate from code not spec, citation requirement for rejections, tentative learnings excluded
- Update worker pool review assignment to check `bundle.disputed` and load `arbitrator.pmt` instead of the normal reviewer prompt; cross-boundary sig injection (Phase 1) runs unconditionally for arbitration
- Add guard in coordinator's rejected-bundle handler (`coordinator.rs:165-210`): skip `override_work` if `bundle.disputed == true` (the arbitrator handles resolution, coordinator must not race it)
- Add retroactive migration in `Stores::open()`: scan all learnings, set `tentative = true` on any with structural markers and no `code_citation`; write back to JSONL
- Unit tests: dispute detection fires on keyword overlap; non-overlap bundles unaffected; arbitrator context lacks work-scoped tentative learnings; coordinator skips override_work for disputed bundles; citation-less reject treated as parse failure and re-queued

## Alternatives Considered

### Alternative 1: Static type checking (mypy/pyright) before reviewer
- **Description:** Run mypy or pyright on the bundle diff before the LLM reviewer runs. If it passes, suppress reviewer's ability to reject on type grounds.
- **Pros:** Grounds type-checking in a deterministic tool rather than LLM intuition.
- **Cons:** Would not have fixed this case — `update_bookmark(db_path, id, **fields)` passes mypy against the actual keyword-arg signature, which is exactly what the implementer was arguing. The reviewer's error was about which version of the signature was authoritative, not about type correctness per se.
- **Why not chosen:** Misdiagnoses the root cause. Doesn't address the learnings institutionalization or the lack of dispute mechanism. Worth adding as defense-in-depth later but not the primary fix.

### Alternative 2: Coordinator reads dependency files during override_work
- **Description:** When the coordinator detects a rejected bundle, have it read the relevant file and adjudicate before resetting to Ready.
- **Pros:** Simpler than a full arbitration path — reuses existing coordinator role.
- **Cons:** Coordinator is already a high-context agent managing many work items; adding per-bundle file reading balloons its context and slows the pipeline. Also requires giving the coordinator read_file capabilities which it doesn't currently have.
- **Why not chosen:** Phase 1 (reviewer reads deps) solves the problem at the right point in the pipeline. Arbitration (Phase 3) is the right fallback when Phase 1 isn't enough.

### Alternative 3: Require implementers to read and quote dependency signatures before committing
- **Description:** Add to implementer prompt: before using any cross-boundary function, read the file and quote the signature in the bundle claims.
- **Pros:** Surfaces conflicts earlier (at implementation time rather than review time).
- **Cons:** The implementer already does this — `ag-6onc7` explicitly wrote "I can see that `database.py`'s `update_bookmark` function takes keyword arguments." The problem is the reviewer ignoring that information.
- **Why not chosen:** The failure is in the reviewer, not the implementer. Adding more burden to the implementer doesn't fix the reviewer's context blindness.

### Alternative 4: Raise attempt_count limit
- **Description:** Allow more than 3 attempts before abandoning.
- **Pros:** Zero implementation cost.
- **Cons:** More attempts doesn't help if each attempt uses the same poisoned context. The 4th attempt would have failed for the same reason.
- **Why not chosen:** Treats the symptom, not the cause.

## Technical Considerations

### Dependencies

- Phase 1 depends on `bundle.head_commit` being populated — it always is for proposed bundles; and on `git show` being available in the execution environment — it always is since the daemon runs in a git repo
- Phase 2 is fully self-contained — `Learning` struct change + context builder change
- Phase 3 depends on Phase 1 (arbitrator needs cross-boundary file hydration) but not Phase 2 (Phase 2 is independent)

### Performance

- Phase 1: Adds file reads at review time. Bounded by token ceiling (3000 tokens). For Python projects with many imports, the resolution step may read several files — acceptable since review is already an LLM call.
- Phase 2: Adds string matching at learning creation and bundle proposal time — negligible.
- Phase 3: Disputed bundles bypass the normal queue and get a dedicated arbitration call. This is an extra LLM call, but only fires on conflict — a rare path that is currently causing abandonment.

### Testing Strategy

- All three phases: unit tests at the module level (struct serialization, detection logic, routing logic)
- Phase 1: integration test — create a bundle that imports a cross-boundary file, verify the file's content appears in the rendered reviewer context
- Phase 2: integration test — create a learning with a structural claim, verify it renders as tentative; verify contradiction increment fires on dispute detection
- Phase 3: integration test — create a prior-rejected bundle + new bundle with overlapping claims, verify `disputed = true` and arbitrator routing

### Rollout Plan

Phases are independently shippable. Recommended order: 1 → 2 → 3. Phase 1 alone prevents the failure from recurring. Phase 2 cleans up poisoned state. Phase 3 is the full defense.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Cross-boundary import resolution fails for complex module structures | Medium | Low | Fail open: if resolution fails, skip hydration for that import; reviewer sees less context, not wrong context |
| Tentative labeling misfires on legitimate structural learnings | Low | Medium | Tentative only means "rendered with caveat" — still visible; high-confidence structural learnings (0.6+) are not suppressed |
| Dispute detection over-triggers on coincidental term overlap | Medium | Low | Disputed bundles get an arbitrator who can still reject; false positives just add an extra LLM call, not a wrong outcome |
| Phase 2 `tentative` auto-promotion too broad/narrow | Medium | Medium | Keyword list is intentionally narrow; tunable from E2E observation without code changes to the detection rule |
| Retroactive migration marks too many existing learnings tentative | Low | Low | Tentative means "render with caveat" not "suppress" - the learning is still visible; false positives only add a label |

## Open Questions

- [ ] Should contradiction increment at bundle-proposal time (eagerly, before any arbitration) or only when the arbitrator confirms the dispute? Eager is safer against poisoning the arbitrator itself but could over-penalize learnings if dispute detection misfires.
- [ ] What is the right semantic overlap heuristic for dispute detection? Keyword intersection on function names / the words "signature", "interface", "contract" is deterministic and auditable. Embedding similarity is more accurate but adds an API call. Start with keyword intersection and measure false-positive rate from E2E runs.
- [ ] Should disputed bundles show a distinct marker in `loopr bundle list` output, or keep `disputed` as an internal routing flag only? Visibility is useful for debugging but adds UI surface area.

## References

- E2E postmortem: python-api run 2026-04-15 (`/tmp/loopr/e2e/python-api/20260415-011101`)
- `src/agents/reviewer.rs:88-202` — `extract_referenced_signatures` (the function being fixed in Phase 1)
- `src/agents/reviewer.rs:242-270` — reviewer context assembly and sig callsite
- `src/agents/reviewer.rs:315-348` — learning creation after review
- `src/domain/learning.rs:44-69` — Learning struct
- `src/agents/context.rs:803-825` — learning injection into context
- `src/agents/context.rs:182-189` — Reviewer token budget
- `src/agents/coordinator.rs:165-210` — rejected bundle handling
- `src/domain/bundle.rs:55-83` — Bundle struct
