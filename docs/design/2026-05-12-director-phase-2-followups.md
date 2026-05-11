# Director Phase 2 follow-up cleanup

**Status:** Implemented
**Crates touched:** loopr, agents, ipc, domain

Four items left on the floor when Director Phase 2 shipped (v0.7.17 -> v0.7.19). Two are real correctness/lifecycle gaps; two are missing test coverage; one is a small operator verb. This doc captures the shape of each, options, and recommended path so the Architect can review the approach before code lands.

## 1. `operator_notifies` lifecycle owner

**Problem.** The Phase 2 design doc named `transition_and_persist_plan` as the chokepoint that inserts a fresh `Arc<Notify>` on `Stalled -> Active` and removes the entry on Plan terminal. The shipped implementation does cleanup at Director-task-exit (RAII) instead, and the new `Stalled -> Active` respawn lives inside `handle_plan_override`, not the transition helper. Functionally fine today; structurally a deviation that will surprise a future reader who reads the design doc.

**Concrete state today:**
- `crates/loopr/src/daemon/startup.rs::startup_reconcile_directors` and `crates/loopr/src/transport/handler.rs::spawn_director_for_plan` insert into `ctx.operator_notifies` before spawning, then the spawned task body removes the entry on Director exit.
- `crates/loopr/src/transport/handler.rs::handle_plan_override` re-spawns the Director inline when the transition is `Stalled -> Active`.
- `crates/loopr/src/daemon/context.rs::transition_and_persist_plan` does not touch `operator_notifies`.

**Options:**

- **A. Move lifecycle into `transition_and_persist_plan`.** Pass `Arc<RwLock<HashMap<PlanId, Arc<Notify>>>>` (or a trait wrapping it) into the helper. Insert on `Stalled -> Active`, remove on any terminal transition. Drops the task-exit cleanup line and the inline respawn in `handle_plan_override`. Faithful to the spec; widest blast radius (every call site of `transition_and_persist_plan`).
- **B. Update the design doc to record the deviation.** Mark items 4 and 5 of the "operator_notify ownership and lifecycle" section as "implemented via task-exit RAII; equivalent under non-panic exit, self-heals on daemon restart." No code change. Lowest cost, but leaves the architectural mismatch.
- **C. Hybrid.** Keep task-exit cleanup as belt-and-suspenders; ADD the `transition_and_persist_plan` removal on terminal transition so the entry clears even if the Director task panics or is aborted. Defense in depth. Removes the inline respawn from `handle_plan_override` and routes it through `transition_and_persist_plan` watching for `Stalled -> Active`.

**Recommendation: C.** The task-exit cleanup is genuinely useful (covers Director panic + shutdown abort); the FSM-side removal closes the spec gap. The `handle_plan_override` respawn moves to `transition_and_persist_plan` as the doc named.

## 2. Missing integration tests (3)

**Problem.** Design doc Phase 9 acceptance list named three integration tests. None of the three shipped.

**The three tests:**
- **Notify wakeup latency.** `loopr director chat` during a 60s idle sleep must wake the Director within roughly 1s.
- **Mid-run Conservative -> Normal demotion.** Director reaches Conservative via repeated `same_action`; an operator note injected mid-run demotes back to Normal before the next iteration.
- **Post-daemon-restart unread-note pickup.** Note arrives while daemon is down; on daemon restart, the new Director's first iteration reads the note from `NotesStore`.

**Options:**

- **A. Ship all three.** All are doable in the existing `crates/loopr/tests/director_chat.rs` harness using `seed_plan` + `loopr daemon start/stop`. The wakeup-latency test needs a configurable short idle interval to avoid waiting 60s; pass via `.loopr/config.yml`. Mid-run demotion needs a background tokio task that pokes the NotesStore from the test process while the daemon runs. Restart pickup needs explicit `daemon stop` -> seed note via Store -> `daemon start` -> verify next Director iteration's `events.log` shows the note.
- **B. Ship only the restart-pickup test.** It is the most semantically distinct; the other two are timing-sensitive and likely to flake. The Notify wakeup is structurally covered by the `tokio::select!` arm being there.
- **C. Defer indefinitely.** Document in the shipped-memory entry that the design doc named these tests and they were never written.

**Recommendation: B.** Restart-pickup is the only test that actually proves a behavior unit tests miss (daemon crash + recovery). The other two are timing-sensitive; the unit-level coverage (`next_mode` + select-arm wiring) is sufficient.

## 3. `loopr director status <plan-id>` CLI verb

**Problem.** Open question in the Phase 2 design doc, deferred at ship time. Operator has no way to see "is this Plan in Conservative? NeedsOperator? what is the no-progress streak?" short of grepping `events.log`.

**Shape of the verb:**

```
$ loopr director status pl-abc12
plan:           pl-abc12
status:         Active
director mode:  Conservative
no-progress:    streak=3 (threshold=8 for escalation)
last action:    override_work wk-xyz Ready  (45s ago)
unread notes:   2
```

**Options:**

- **A. Add a new IPC verb `director.status`.** Daemon's per-Plan Director task exposes mode + streak via a sidecar handle on `DaemonContext` (parallel to `operator_notifies`). Cheapest path to live data.
- **B. Derive status from `events.log`.** Client-side: grep the latest `director.mode_change` event for the Plan, plus pending notes. No daemon-side wiring; cheaper to build but stale by definition.
- **C. Defer again.** Operator can already use the grep cookbook recipes from `docs/telemetry-grep-cookbook.md` to answer the same questions.

**Recommendation: A.** It is small (one IPC verb, one CLI verb, parallel to `director.chat`). Live data matters when the operator is about to send a `director chat` to fix things; otherwise they are racing the log.

## Suggested execution order

1. **#2B** (restart-pickup integration test) - small, isolated, no design risk.
2. **#1C** (lifecycle owner refactor) - moderate blast radius (every `transition_and_persist_plan` caller), but bounded.
3. **#3A** (`director status` verb) - new surface; ship.

## Open questions for the Architect

- Is #1C the right tradeoff, or does the FSM-side lifecycle owner introduce coupling that the original Phase 9 task-exit-cleanup design was deliberately avoiding?
- Is #2B sufficient, or are #2A's wakeup-latency and mid-run-demote tests load-bearing for the spec's intent in ways the unit tests do not cover?
- Is #3A's sidecar `Arc<RwLock<HashMap<PlanId, DirectorStatusSnapshot>>>` the right shape, or should the Director publish its status to TaskStore on every iteration (durable + queryable by anyone, but adds a per-iteration write)?

---

## Addendum: Architect review outcome (2026-05-11)

Reviewed by Gemini Architect persona; verdicts incorporated below. The recommendations in items #1, #2, and #3 are superseded by this addendum; the body of each item is preserved as the option-space write-up.

### Item 1: superseded by 1B (no code change)

Architect rejected Hybrid (1C). The two-line argument:

1. **`transition_and_persist_plan` is a pure FSM helper.** It currently takes `S: store::PlanUpdateSink` so it is decoupled from `DaemonContext` and from task spawning. Threading `operator_notifies` (and the spawn surface) through it pollutes the abstraction. The pure helper survives precisely because it has no awareness of orchestration.
2. **Task-exit RAII is architecturally superior, not a workaround.** It handles panics and shutdown aborts natively. Tying removal to FSM terminal transitions would leak the `Arc<Notify>` whenever a Director panicked before reaching a terminal state. The shipped behavior is correct.

**Decision:** Keep the code as shipped. Update the Phase 2 design doc's "operator_notify ownership and lifecycle" section (items 4 and 5) to record that task-exit RAII is the actual mechanism, equivalent under non-panic exit and self-healing on daemon restart. The respawn on `Stalled -> Active` stays in `handle_plan_override` next to the FSM override call site.

**Implementation:** edit `docs/design/2026-05-09-director-phase-2.md`. No source-code change.

### Item 2: confirmed 2B (restart-pickup test only)

Architect agreed with the timing-sensitivity argument. Wakeup-latency and mid-run-demote are race conditions across the daemon process boundary; the `tokio::select!` arm in `run_director_inner` plus the `next_mode` unit tests already prove the behavior statically. The restart-pickup test exercises the only gap unit tests miss: note persistence across the daemon cold-boot boundary via `startup_reconcile_directors` + first-iteration `list_unread_notes_for_plan`.

**Decision:** Ship one integration test, `crates/loopr/tests/director_chat.rs::note_persists_across_daemon_restart` (or similar name). Skip the other two; document the skip in this addendum so a future reader does not re-litigate.

**Implementation shape:**
1. `seed_plan(target, "restart-target")` -> persisted Plan id.
2. Auto-fork daemon via `loopr plans` (cheap, no LLM).
3. Use `store::Store::open` from the test process to seed an `OperatorNote` for the Plan (daemon is running, but the JSONL write is append-only and `notes` has no OCC, so a direct write is safe for the test scope).
4. Issue `loopr daemon stop`, wait for socket teardown.
5. Re-issue `loopr daemon start` (the next client request auto-forks; `loopr plans` again is sufficient).
6. Wait briefly (poll the daemon's `events.log` for `director iteration start`) for the new Director's first iteration.
7. Re-open the Store from the test process and assert the seeded note's `read_at` is `Some(_)`, proving the post-restart Director ingested it.

The seed-via-Store + read-via-Store pattern matches `crates/loopr/tests/director_chat.rs`'s existing approach.

### Item 3: confirmed 3A with required additions

Architect agreed sidecar-on-DaemonContext is correct. Persisting a per-iteration `DirectorStatusSnapshot` to TaskStore contradicts Phase 2's Alternative 3 rejection: Director mode is ephemeral, resets to Normal on restart, and a 5s-cadence write would pollute the JSONL. The sidecar entry is naturally absent when no Director is running, which is the right semantics.

Architect required two specifications the original doc skipped: **snapshot field schema** and **lock discipline**.

**Snapshot schema** (`crates/agents/src/director.rs::DirectorStatusSnapshot`):

```rust
pub struct DirectorStatusSnapshot {
    pub mode: DirectorMode,
    pub no_progress_streak: u32,
    pub same_action_streak: u32,             // derived from tracker on snapshot
    pub iteration: u32,
    pub last_action_kind: Option<&'static str>,    // accept_bundle / override_work / assign_work / done / need_help
    pub last_action_target_id: Option<String>,     // bundle_id / work_id; None for done/need_help
    pub last_action_ts: Option<i64>,               // millis
    pub unread_note_count: usize,
    pub needs_operator_iters: u32,                 // Phase 10 grace counter
}
```

`DirectorMode` is already `Copy + Serialize`; this struct derives `Clone + Serialize`. Snapshot is built at the END of each `run_director_inner` iteration, AFTER the pattern tracker observation runs.

**Lock discipline** (`DaemonContext::director_statuses: Arc<RwLock<HashMap<PlanId, DirectorStatusSnapshot>>>`):

- Director task acquires `write()` once per iteration, near the end of the loop body (after the pattern tracker block, before the sleep). Writes the freshly-built snapshot with `.insert(plan_id.clone(), snapshot)`. Lock is held only for the duration of the insert; no `.await` between acquire and drop.
- IPC handler (`handle_director_status`) acquires `read()`, clones the snapshot if present, drops the lock, then constructs the response. No lock held across serde or socket I/O.
- Director-exit cleanup (in the spawned task body, mirroring `operator_notifies`): remove the entry on Director task exit. Same RAII pattern as #1.

**IPC verb** (`crates/ipc/src/method.rs`):

- `Method::DirectorStatus(DirectorStatusParams { plan_id })` -> `MethodName::DirectorStatus` wire form `director.status`.
- `DirectorStatusResult { plan_id, plan_status, snapshot: Option<DirectorStatusSnapshot> }`. `snapshot: None` means the Plan exists but has no live Director (Stalled, Complete, or transient pre-spawn).

**CLI verb** (`crates/loopr/src/cli.rs::DirectorCmd::Status { plan_id }`):

```
$ loopr director status pl-abc12
plan:           pl-abc12
status:         Active
director mode:  Conservative
no-progress:    streak=3 (escalation threshold=8)
same-action:    streak=2 (threshold=3)
last action:    override_work wk-xyz Ready  (45s ago)
unread notes:   2
iteration:      14
needs-operator: 0 iters (grace=5)
```

When `snapshot: None`, the CLI prints `director: not running (plan is <status>)` and returns success.

## Revised execution order

1. **Item 2 (restart-pickup integration test)** — small, isolated, no design risk.
2. **Item 3 (director status verb)** — three crates touched (ipc, loopr, agents); follows the `director.chat` pattern shipped in Phase 8.
3. **Item 1 (design doc update)** — pure documentation; can fold into the same shipping commit as items 2 or 3.

## Phases

### Phase 1: restart-pickup integration test (Item 2)

**Model:** sonnet

- Add `note_persists_across_daemon_restart` to `crates/loopr/tests/director_chat.rs` following the shape in the Item 2 decision section above.
- Use the existing `DaemonAutoStop` panic-safe guard and `stop_daemon_for` helper.
- Use `seed_plan` + `loopr plans` to auto-fork the daemon; do not route through `plan create` (real LLM path is the source of the existing flake guarded against in `stage_7_wiring.rs`).
- The test must verify the note's `read_at` is `Some(_)` after the post-restart Director's first iteration completes. Poll the daemon's `events.log` for the iteration-start marker rather than sleeping.

### Phase 2: DirectorStatusSnapshot sidecar + IPC + CLI (Item 3)

**Model:** sonnet

- Add `DirectorStatusSnapshot` struct in `crates/agents/src/director.rs` with the schema above. `#[derive(Clone, Debug, Serialize)]`.
- Add `director_statuses: Arc<RwLock<HashMap<PlanId, DirectorStatusSnapshot>>>` to `DaemonContext` in `crates/loopr/src/daemon/context.rs`. Insert + remove lifecycle exactly mirrors `operator_notifies`: insert before spawn, remove at task exit.
- Pass `director_statuses` (and the per-Plan write closure) into `DirectorDeps`. The Director loop builds + writes the snapshot at the end of every iteration after the pattern tracker block.
- Add `Method::DirectorStatus` / `MethodName::DirectorStatus` / `DirectorStatusParams` / `DirectorStatusResult` in `crates/ipc/src/method.rs`. Wire name `director.status`. Re-export from `crates/ipc/src/lib.rs`. Seam tests in `crates/ipc/src/tests.rs` (wire name, try_from, deny_unknown_fields).
- Add `handle_director_status` in `crates/loopr/src/transport/handler.rs`. Validates Plan exists, reads sidecar, returns snapshot or `None`. Three unit tests: happy path with live snapshot, no-snapshot (Plan exists but Stalled), missing Plan -> NotFound.
- Add `DirectorCmd::Status { plan_id }` to `crates/loopr/src/cli.rs`. Wire CLI handler in `crates/loopr/src/commands/director.rs` next to `chat`. Format matches the section above.
- Update `crates/agents/CLAUDE.md` and `crates/loopr/CLAUDE.md` to mention the new sidecar + verb.
- Tests for the snapshot builder: a unit test in `crates/agents/src/director/tests/operator.rs` (or a new sibling test file) that drives `run_director_inner` for a few iterations and asserts the snapshot in the sidecar reflects the latest `current_mode` + streak.

### Phase 3: design doc + memory update (Item 1 + closeout)

**Model:** sonnet

- Edit `docs/design/2026-05-09-director-phase-2.md` "operator_notify ownership and lifecycle" section (items 4 and 5): record that task-exit RAII is the actual mechanism. Keep the existing prose; append an "Implementation note (2026-05-11)" paragraph explaining the architectural rationale (panic resilience, abstraction purity for `transition_and_persist_plan`).
- Update `~/.claude/projects/-home-saidler-repos-scottidler-loopr/memory/project-director-phase-2-shipped.md`: move the `transition_and_persist_plan` and `loopr director status` entries from the "known gaps" section into a new "follow-ups landed (v0.7.20+)" section. Reflect the test gap closure for restart-pickup.
- Update `docs/telemetry-grep-cookbook.md` with one new recipe: "Which Plans have a live Director? -> `loopr director status` per-Plan, or `grep director_statuses.insert` in the daemon log."
- Mark this doc `Status: Implemented`.
- Bump (`/bump`), push, install.
