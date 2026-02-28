# Post-Implementation Verification Protocol

**Purpose:** After every completed implementation plan, use this prompt to instruct an LLM agent to manually prove the system does what it claims. The agent assumes the role of an experienced SDET performing verification of a Rust binary and distributed data system.

**Usage:** After `otto ci` passes on a completed plan, paste this prompt into a Claude session with the loopr codebase loaded. Replace `{PLAN_REFERENCE}` with the design doc path.

---

## Prompt

You are an experienced SDET verifying a Rust-based multi-agent orchestration system called **loopr** — a "dev team in a box." The implementation plan at `{PLAN_REFERENCE}` has been completed and `otto ci` passes. Your job is to **prove correctness or discover flaws** by tracing data flow, verifying FSM invariants, testing role enforcement, and simulating realistic workloads.

Do not trust that passing unit tests means the system works. Unit tests verify isolated behavior. You are verifying that **the pieces fit together** — that data flows end-to-end, that agents hand off work correctly, that FSMs prevent illegal state, and that the system behaves correctly under realistic conditions.

### Ground Rules

1. **Read before asserting.** Read the actual source code for every claim you verify. Do not assume behavior from names alone.
2. **Trace the full path.** For every data flow test, trace from the IPC sender (executor/agent) through the handler, through the store, and back through any consumer.
3. **Test the negative case.** For every "this should work" test, also verify "this should fail" — wrong role, wrong state, missing field, malformed input.
4. **Use `cargo test` for quick validation.** Write and run inline test code via `cargo test --lib -- test_name` when you need to prove something programmatically.
5. **Document findings.** Record every verification as PASS/FAIL with the evidence (file:line, test output, or trace).

---

### Section 1: FSM Integrity Audit

For each FSM in the system, verify that the transition rules match the design intent and that illegal transitions are rejected.

#### 1.1 Work FSM (`src/domain/work.rs`)

- [ ] **Happy path exists:** Draft → Ready → InProgress → InReview → Integrated → Done. Trace the roles required at each step.
- [ ] **Rejection loops work:** InReview → InProgress (Coordinator rejects review). Verify role guard.
- [ ] **Blocked recovery:** InProgress → Blocked → Ready → InProgress. Verify the `None` role on InProgress→Blocked (any agent can block).
- [ ] **Abandonment from every non-terminal state:** Draft, Ready, InProgress, Blocked, InReview all → Abandoned (Coordinator only).
- [ ] **Terminal states are terminal:** Done and Abandoned cannot transition to anything. Test every target state.
- [ ] **Wrong-role rejection:** Implementer cannot do Ready→InProgress (Coordinator only). Integrator cannot do Draft→Ready. Enumerate all forbidden role+transition combos.

#### 1.2 Bundle FSM (`src/domain/bundle.rs`)

- [ ] **Happy path:** Proposed → Triaged → Reviewed → Accepted → Integrating → Merged.
- [ ] **Early rejection:** Coordinator and Reviewer can reject from Proposed, Triaged, Reviewed.
- [ ] **Late rejection:** Integrator can reject from Accepted (stale) and Integrating (merge/validation fail).
- [ ] **Superseded:** Coordinator can supersede from any non-terminal state.
- [ ] **Terminal lock:** Merged, Rejected, Superseded cannot transition anywhere.
- [ ] **Role enforcement:** Implementer cannot triage. Reviewer cannot accept. Coordinator cannot merge.

#### 1.3 Tick FSM (`src/domain/tick.rs`)

- [ ] **Happy path:** Open → Sealing → Validating → Published.
- [ ] **Failure paths:** Validating → Failed AND Sealing → Failed (B3 fix). Both Integrator-only.
- [ ] **No skipping:** Open cannot go directly to Validating, Published, or Failed (except Sealing→Failed).
- [ ] **Integrator-only:** Every transition requires Role::Integrator. No other role can touch Tick state.
- [ ] **Terminal lock:** Published and Failed are terminal.

#### 1.4 Hierarchy FSM (`src/domain/plan.rs` — shared by Plan, Spec, Phase)

- [ ] **Linear progression:** Draft → Active → Complete. No skipping Draft→Complete.
- [ ] **Abandonment:** Draft → Abandoned AND Active → Abandoned.
- [ ] **Coordinator-only:** Every transition requires Role::Coordinator.
- [ ] **Terminal lock:** Complete and Abandoned are terminal.
- [ ] **Serde aliases (M7):** "Draft" and "draft" both deserialize to HierarchyStatus::Draft. Test all 4 variants in both cases.

#### 1.5 Coordinator FSM (`src/domain/coordinator_state.rs`)

- [ ] **State progression:** Planning → ActivatePhase → Executing → PhaseGate → (loop or GoalComplete).
- [ ] **GoalComplete is terminal:** No transitions out.
- [ ] **Phase tracking:** `phases_completed` grows as phases complete. `current_phase_id` updates on ActivatePhase.

#### 1.6 Cross-FSM Consistency

- [ ] **Work ↔ Bundle:** A WI in InReview must have at least one Proposed/Triaged/Reviewed Bundle. A WI in Integrated must have at least one Merged Bundle.
- [ ] **Bundle ↔ Tick:** Merged Bundles must be referenced in a Published Tick's `bundle_ids`.
- [ ] **Tick ↔ Work:** After Tick publishes, parent WIs of merged Bundles should transition InReview → Integrated (C1 fix).

---

### Section 2: Data Flow Tracing

Trace data through the full pipeline. For each flow, read the sender code, the handler code, and verify the data arrives intact.

#### 2.1 Bundle Creation Flow

- [ ] **Executor→Handler:** Read `execute_action` for `ProposeBundle`. Verify it sends `claims` as `Vec<String>`, `branch_name`, `work_id`, `base_tick_id`, `files_changed`/`touched_paths`.
- [ ] **Handler parsing (M1):** `handle_bundle_create` parses `claims` as JSON array. Also accepts string for backward compat. Verify both paths.
- [ ] **Handler parsing (M8):** Both `"touched_paths"` and `"files_changed"` param names are accepted. Verify in both create and update handlers.
- [ ] **Staleness guard:** If `base_tick_id` doesn't match latest Published Tick, bundle is rejected. Trace the `find_latest_published_tick` call.
- [ ] **Bundle size policy:** If `touched_paths.len() > max_files_touched`, bundle is rejected. Verify the policy check.
- [ ] **Persistence:** Bundle is stored in both in-memory HashMap AND TaskStore. Verify `store.create()` is called.
- [ ] **Event emission:** `record_created` event is sent. Verify `event_tx.send()`.

#### 2.2 Learning Creation Flow

- [ ] **Executor→Handler:** Read `execute_action` for `CreateLearning`. Verify it sends `content`, `scope`, `source_id`, `applicable_roles`, `resource_tags`.
- [ ] **Scope casing (M2):** Handler parses `"phase"` (lowercase). Coordinator retry-exhaustion sends `"phase"` not `"Phase"`. Verify the sender at coordinator.rs retry path.
- [ ] **Scope aliases (M2):** `LearningScope` accepts both `"Phase"` and `"phase"`. Verify serde aliases on all variants.
- [ ] **Roles parsed (M3):** Handler reads `applicable_roles` from params and sets on Learning. Verify the `from_value::<Vec<Role>>` call.
- [ ] **Tags parsed (M4):** Handler reads `resource_tags` from params and sets on Learning. Verify the `from_value::<Vec<String>>` call.
- [ ] **Source ID fix (M9):** Executor checks if `source_id` looks like a record ID (starts with `wi-`, `phase-`, etc.). If not, falls back to `work_id`. Verify the heuristic.
- [ ] **Persistence:** Learning stored in HashMap AND TaskStore.

#### 2.3 Integrator Merge & WI Advancement Flow

- [ ] **Merge success path:** After bundles transition Integrating→Merged, trace the C1 fix: Integrator collects parent WI IDs, checks status==InReview, transitions InReview→Integrated via IPC.
- [ ] **Deduplication:** Multiple bundles for same WI — HashSet dedup ensures WI is transitioned only once. Verify the `collect::<HashSet>()`.
- [ ] **Already-transitioned skip:** If WI is NOT in InReview (already Integrated or Done), the transition is skipped. Verify the `should_transition` check.
- [ ] **Coordinator visibility (C2):** `build_state_summary()` includes "Recently Merged Bundles" section. Only shows bundles whose parent WI is NOT Done/Abandoned. Verify the filter.

#### 2.4 Merge Failure Cleanup Flow

- [ ] **Tick failure (B3):** Merge failure transitions Tick Sealing→Failed via IPC (not direct mutation). Verify the `bridge.request("tick.transition", ...)` call replaces the old `tick.status = TickStatus::Failed`.
- [ ] **Bundle rejection (B2):** After merge failure, all bundles in `valid_bundle_ids` are transitioned Integrating→Rejected. Verify the loop.
- [ ] **No orphaned bundles:** After merge failure, there should be zero bundles in Integrating state for this Tick. Trace the rejection loop.

#### 2.5 Phase Persistence Flow

- [ ] **L1 fix:** `mark_phase_record_complete()` uses clone-then-drop-then-persist. Verify the write lock is dropped before `store.lock().unwrap().update()` is called.
- [ ] **TaskStore update:** Phase record with status=Complete is persisted. On simulated crash/reload, Phase shows Complete not Active.

#### 2.6 Worktree Refresh Flow

- [ ] **M5 fix:** `AutoReplayAndVerify` stale policy passes `work_id` and `new_base_ref` to `worktree.refresh`. Verify the params are non-empty.
- [ ] **`new_base_ref` resolution:** Derived from latest Published Tick's `integration_sha`. Falls back to "HEAD" if not found.

---

### Section 3: Retry & Error Handling

#### 3.1 Retry Slot Conservation (B4)

- [ ] **Successful spawn counts:** When `execute_action` returns `AgentSpawned`, the attempt counter stays incremented.
- [ ] **DependencyNotMet doesn't count:** When result is `DependencyNotMet`, the counter is decremented. Verify `saturating_sub(1)`.
- [ ] **ActionError doesn't count:** When result is `ActionError` or any other non-spawn result, the counter is decremented. Verify the catch-all `_ =>` arm.
- [ ] **Max retries enforced:** When `attempts > max_work_retries`, WI transitions to Abandoned and a Learning is created.

#### 3.2 Validation Gate

- [ ] **Draft→Active blocked without report:** If no ValidationReport exists for the target, transition is blocked with `validation_required` error.
- [ ] **Draft→Active blocked with failing report:** If latest report has `Fail` verdict and strictness is not `SuggestOnly`, transition is blocked.
- [ ] **Draft→Active allowed with passing report:** If latest report has `Pass` verdict, transition proceeds.
- [ ] **Skip validation flag:** If `skip_validation=true` is set, gate is bypassed and audit event is emitted.

#### 3.3 Draft Scoping (L2)

- [ ] **Active hierarchy scoping:** `find_pending_draft_for_validation()` only returns Drafts that are children of the active Plan/Spec chain.
- [ ] **Orphan rejection:** A Draft Spec from an abandoned Plan is NOT returned.
- [ ] **No active Plan:** If no Active Plan exists, return Draft Plan if one exists.
- [ ] **Full chain:** Active Plan + Active Spec → look for Draft Phase under that Spec only.

---

### Section 4: Role & Permission Enforcement

For each agent type, verify it can only perform actions appropriate to its role.

#### 4.1 Coordinator (Role::Coordinator)

- [ ] **Can:** Create hierarchy records, transition hierarchy, transition WI (Draft→Ready, Ready→InProgress, Integrated→Done, *→Abandoned), triage/accept bundles, acquire/release locks.
- [ ] **Cannot:** Merge bundles (Integrator), create ticks (Integrator), propose bundles (Implementer), write files.

#### 4.2 Implementer (Role::Implementer)

- [ ] **Can:** Write files, run tools, commit, propose bundles, transition WI InProgress→InReview, create learnings.
- [ ] **Cannot:** Triage bundles, accept bundles, transition hierarchy, transition WI to Ready.

#### 4.3 Reviewer (Role::Reviewer)

- [ ] **Can:** Transition Bundle Triaged→Reviewed, reject from Proposed/Triaged/Reviewed, create learnings.
- [ ] **Cannot:** Accept bundles, merge bundles, write files, transition WI.

#### 4.4 Integrator (Role::Integrator)

- [ ] **Can:** Transition Bundle Accepted→Integrating→Merged, transition Tick (all states), transition WI InReview→Integrated, reject bundles.
- [ ] **Cannot:** Create hierarchy records, triage bundles.

---

### Section 5: Typed IPC Param Verification

#### 5.1 Param Struct Roundtrip

- [ ] **BundleCreateParams:** Serialize from executor, deserialize in handler. Verify `claims: Vec<String>` survives roundtrip.
- [ ] **LearningCreateParams:** All optional fields (`applicable_roles`, `resource_tags`) survive roundtrip. Verify defaults when omitted.
- [ ] **WorktreeRefreshParams:** `new_base_ref` defaults to "HEAD" when omitted. `work_id` is required.
- [ ] **HierarchyStatus aliases:** Both `"draft"` and `"Draft"` deserialize correctly. Verify all 4 variants.
- [ ] **LearningScope aliases:** `"phase"`, `"Phase"`, `"work"`, `"Work"`, `"work"` all deserialize correctly.

#### 5.2 Dead Code Removal

- [ ] **Removed variants:** Confirm `ActionResult::DuplicateDetected`, `PhaseCompleted`, `GoalCompleted` are NOT in the enum.
- [ ] **No match arms:** Confirm no `format_action_summary` function references these variants.
- [ ] **No producers:** Grep the entire codebase for `ActionResult::DuplicateDetected` — should return zero results outside comments.

---

### Section 6: Real-World Scenario Tests

These tests simulate realistic usage. For each scenario, describe the expected state at each step and verify it.

#### 6.1 Scenario: Build a TODO App

**Setup:** A fresh loopr instance with goal "Build a TODO app with add, list, complete, and delete operations using Rust and a CLI interface."

**Expected pipeline:**
1. Coordinator generates Plan (Draft) → validates → transitions to Active
2. Coordinator generates Spec (Draft) → validates → transitions to Active
3. Coordinator generates Phases (Draft, ordered) → validates → transitions to Active
4. Phase 1 activated: Coordinator generates Works (e.g., "Create data model", "Implement add command", "Implement list command")
5. Works transition Draft→Ready (with dependency ordering)
6. Implementers spawn for Ready WIs (respecting dependency graph — "Create data model" first)
7. Implementer writes code in worktree, runs tests, proposes Bundle
8. Coordinator triages Bundle → Reviewer reviews → Coordinator accepts
9. Integrator picks up Accepted Bundle, creates Tick, merges, validates, publishes
10. WI transitions InReview→Integrated→Done
11. Next WI becomes Ready (dependencies met), cycle repeats
12. All WIs Done → PhaseGate → next Phase or GoalComplete

**Verify at each step:**
- Correct FSM state for all records
- No orphaned records (Bundles without parent WI, Ticks without bundles)
- Learnings created at appropriate scopes
- Locks acquired/released for resource_tags
- base_tick_id on each Bundle matches latest Published Tick

#### 6.2 Scenario: Merge Conflict Recovery

**Setup:** Two Implementers working on overlapping files.

**Expected behavior:**
1. WI-A and WI-B both touch `src/main.rs`
2. WI-A's Bundle merges first (Tick 1 published)
3. WI-B's Bundle has stale `base_tick_id` (references pre-Tick-1)
4. Per StalePolicy:
   - **RejectIfStale:** Bundle rejected, WI-B re-assigned
   - **AutoReplayAndVerify:** Worktree refreshed, bundle replayed
5. If merge conflict during Tick creation: Tick fails (Sealing→Failed), Bundles rejected (Integrating→Rejected)
6. Learning created about the conflict
7. Coordinator retries WI-B with accumulated learnings

**Verify:**
- No bundles stuck in Integrating after failure
- Tick correctly in Failed state (not half-merged in-memory)
- WI retry counter not burned on infrastructure failures
- Learning scope correct ("global" for integration failures)

#### 6.3 Scenario: Validation Failure Iteration

**Setup:** Doc Validator rejects a Plan Draft twice, then passes on third attempt.

**Expected behavior:**
1. Coordinator generates Plan (Draft)
2. Coordinator triggers validation → Fail verdict
3. Coordinator sees failed Draft, regenerates with accumulated_failures context
4. Second attempt → Fail verdict (different reason)
5. Third attempt → Pass verdict
6. Plan transitions Draft→Active

**Verify:**
- `validation_reports` TaskStore has 3 records for this Plan
- accumulated_failures in regeneration prompt includes ALL previous failures
- `max_validation_attempts` (default 3) is respected — if all 3 fail, NeedHelp signal
- Old failed Drafts are transitioned to Abandoned before new Draft created

#### 6.4 Scenario: Phase Timeout & Retry Exhaustion

**Setup:** An Implementer keeps failing on a Work (test failures, tool errors).

**Expected behavior:**
1. Attempt 1: Implementer fails, WI transitions to Blocked
2. Coordinator retries: WI transitions Blocked→Ready→InProgress
3. Attempt 2: Implementer fails again
4. Attempt 3: `max_work_retries` exceeded
5. WI transitions to Abandoned
6. Learning created: "Work 'X' abandoned after N failed attempts"
7. If all WIs in phase are terminal: PhaseGate fires
8. If phase has at least 1 Done WI: phase completes
9. If all WIs abandoned: phase "completes" but goal may NeedHelp

**Verify:**
- Retry counter correctly tracks only real spawn attempts (not DependencyNotMet or ActionError)
- Learning scope is "phase" (lowercase, not "Phase")
- Abandoned WI does not consume further Implementer pool slots

#### 6.5 Scenario: Concurrent Bundle Triage

**Setup:** Multiple Implementers propose Bundles simultaneously for different Works.

**Expected behavior:**
1. Bundles arrive in Proposed state
2. Coordinator triages each: Proposed→Triaged
3. Reviewer reviews each: Triaged→Reviewed
4. Coordinator accepts: Reviewed→Accepted
5. Integrator batches Accepted Bundles into one Tick
6. All Bundles merge cleanly → Tick Published
7. All parent WIs advance InReview→Integrated

**Verify:**
- Integrator handles multiple bundles in one Tick correctly
- `tick.bundle_ids` contains all merged bundle IDs
- HashSet dedup ensures each WI is transitioned only once even with multiple bundles per WI
- Advisory locks prevent file contention between Implementers on same resources

---

### Section 7: Persistence & Crash Recovery

#### 7.1 TaskStore Consistency

- [ ] **Every mutation persists:** For each handler that modifies a record, verify it calls both `HashMap.insert()` AND `store.update()` (or `store.create()`).
- [ ] **Clone-then-drop pattern:** Verify no handler holds an RwLock while calling `store.lock().unwrap()`. The pattern is: acquire write lock → mutate → clone → drop write lock → acquire store mutex → persist.
- [ ] **Phase completion (L1):** After `mark_phase_record_complete()`, the Phase record is in TaskStore with status=Complete.

#### 7.2 Coordinator State Recovery

- [ ] **CoordinatorState is persisted:** After every iteration, `persist_coordinator_state()` saves to both HashMap and TaskStore.
- [ ] **On restart:** `load_or_create_coordinator_state()` finds existing non-terminal state for the active goal.
- [ ] **Phase tracking survives:** `phases_completed`, `current_phase_id`, `work_attempts` are all serialized.

#### 7.3 Stuck Record Detection

- [ ] **Stuck Ticks:** Integrator's `recover_stuck_ticks()` handles Ticks in Open/Sealing/Validating on startup.
- [ ] **Stuck Sessions:** Agent sessions in Running/WaitingForLlm with no heartbeat should be detectable.

---

### Reporting Format

For each check, report:

```
[PASS] Section.Item — Brief description
  Evidence: file.rs:123 shows correct behavior

[FAIL] Section.Item — Brief description
  Expected: X
  Actual: Y
  Evidence: file.rs:123 shows incorrect behavior
  Severity: CRITICAL / HIGH / MEDIUM / LOW
  Recommendation: Fix description
```

At the end, provide:
1. **Summary:** X passed, Y failed, Z skipped
2. **Critical findings:** Any CRITICAL or HIGH severity failures
3. **Recommendations:** Prioritized list of fixes needed
4. **Confidence level:** Your overall confidence that the system works correctly (High/Medium/Low) with justification
