# Design Document: Director Phase 2 - Stuck-State Detection and Judgment Plane

**Author:** Scott A. Idler
**Date:** 2026-05-09
**Status:** Implemented
**Crates touched:** domain, store, agents, context, ipc, loopr
**Review Passes Completed:** 5/5

---

## Summary

Director Phase 2 closes the four items the Phase 1 design doc explicitly punts to a follow-up: three deterministic stuck-state cases the reconcile sweep cannot catch with Phase 1's `WorkSpawner` surface (stuck `Triaged` Bundles, stuck `Accepted` Bundles, stuck `InProgress` Works), and the LLM judgment plane Phase 1 listed as a Non-Goal (pattern tracker, escalation modes, user-intervention chat). Eleven phases, dependency-ordered, so the deterministic stuck-state work lands first as a stable surface before the judgment plane and the operator channel build on top.

---

## Problem Statement

### Background

Director Phase 1 shipped in v0.7.11 as a per-Plan Opus supervisor. Its reconcile sweep handles `Integrated -> Done` promotion and the `GoalComplete` audit. Its action vocabulary covers `AcceptBundle` / `OverrideWork` / `AssignWork` / `Done` / `NeedHelp`. The Phase 1 follow-ups doc (2026-05-09) closed the cold-boot loop with `PlanStatus::Stalled` and the `max_work_attempts` retry budget.

Phase 1's design doc explicitly defers four items to Phase 2. Three are flagged in the reconcile-sweep section under "Deferred to Phase 2"; one is in Non-Goals as "Director Phase 2 judgment plane (3.1)":

1. **Stuck `Triaged` Bundles** (Triaged with no live Reviewer task). Phase 1 has no `spawn_reviewer` on `WorkSpawner`; the reconcile sweep cannot recover this state.
2. **Stuck `Accepted` Bundles** (Accepted with no live Integrator task). Phase 1's `accept_bundle` is idempotent at the FSM level, but its already-Accepted branch early-returns *without* spawning the Integrator (`crates/loopr/src/daemon/context.rs:1253-1257`). So re-emitting `AcceptBundle` from the LLM does nothing for an Accepted-no-Integrator state. A new method is required.
3. **Stuck `InProgress` Works** (InProgress with no live Implementer task, e.g. Implementer panicked without FSM cleanup). Detection requires `WorkSpawner` to expose live-task IDs (the Phase 1 doc names this `WorkSpawner::list_running_work_ids`).
4. **Judgment plane**: pattern tracker (cross-iteration "no progress" detection beyond the per-Work `attempt_count` cap), escalation modes (Director adapts strategy as it observes patterns), user-intervention chat (operator messages routed into the Director's history).

### Problem

Phase 1 left the daemon brittle to two classes of failure:

- **Crash-interrupted state.** Three FSM states (Triaged Bundle, Accepted Bundle, InProgress Work) require a live tokio task to make progress. A daemon crash mid-spawn, an Implementer panic, or a Reviewer crash leaves the FSM in a state nothing reads. Phase 1's reconcile sweep handles only `Integrated -> Done` promotion; it cannot detect or recover the other three.
- **No judgment beyond the prompt.** Phase 1 trusts the LLM. The `attempt_count` retry budget caps per-Work loops, but cross-Work patterns ("Director has emitted `OverrideWork` against five different Works in the last ten iterations and none have made progress") are invisible to both the prompt and the existing lifeguard. The operator has no channel to inject context ("the build is failing on flaky tests; retry the next Bundle anyway"). The prompt is fixed; the Director cannot adapt strategy when it observes a pattern.

### Goals

- Reconcile sweep detects and recovers the three deterministic stuck states. Each has a deterministic re-fire path (re-spawn Reviewer, re-spawn Integrator, transition Work back to Ready).
- `WorkSpawner` grows the surface needed: `spawn_reviewer(bundle_id)`, `spawn_integrator(bundle_id)`, `list_running_work_ids() -> Vec<WorkId>`, `list_running_reviewer_bundle_ids() -> Vec<BundleId>`, `list_running_integrator_bundle_ids() -> Vec<BundleId>`.
- A grace window (`reconcile_grace_secs`, default 30s) gates recovery against the existing `updated_at` field on Work / Bundle, so a Work / Bundle that JUST entered its current status doesn't get clobbered by a recovery that races the spawn-chain.
- Pattern tracker observes the Director's emitted actions and the Plan's `state_hash` across iterations. When a pattern fires, the Director shifts mode.
- Escalation modes (`Normal`, `Conservative`, `NeedsOperator`) drive a label in the state user message. The system prompt has a fixed mode-aware section the LLM matches against. The system prompt itself stays byte-stable across mode transitions (cache-locality rule).
- User-intervention chat: an operator can send `loopr director chat <plan-id> "<message>"`. The message persists to TaskStore. The Director's next iteration prepends unread messages to its state user message before the LLM call.
- Operator messages and mode transitions surface in events (`director.mode_change`, `director.operator_note`).

### Non-Goals

- **Replacing Phase 1's outer loop or restart machinery.** Phase 2 extends `reconcile_director` and `run_director_inner`; the outer loop, `max_restarts`, and the parse-retry sub-loop stay.
- **Replacing Phase 1's `attempt_count` retry budget.** The Layer-1 increment site and Layer-2/3 caps from the Phase 1 follow-ups doc remain. The pattern tracker is an *additional* cross-iteration safety net, not a replacement.
- **Researcher spawning, re-decomposition, parallel Implementers.** All explicitly listed as Non-Goals in Phase 1; remain Non-Goals here.
- **Operator UI beyond the CLI.** No web UI, no TUI integration. `loopr director chat` is a one-shot CLI message.
- **Mode persistence across Director restarts.** Mode is in-memory per Director task. A restart reverts to `Normal`; the pattern tracker re-observes from the fresh history. Restarts are rare (max 3 per Phase 1) and the pattern tracker re-warms within a few iterations.
- **Stage 9 regression.** The Phase 1 seam test must continue passing unchanged. Phase 2 is additive.

---

## Proposed Solution

### Overview

Eleven phases, dependency-ordered. Phases 1-3 are the deterministic stuck-state work and ship as a coherent first slice. Phases 4-6 are the pattern tracker + escalation modes. Phases 7-10 are the operator-channel plane. Phase 11 wraps.

| Phase | Scope | Depends on |
|---|---|---|
| 1 | `WorkSpawner` trait extension; `DaemonContext` sidecar maps; spawn-wrapper RAII tracking | - |
| 2 | `DaemonSpawner` impl of the five new `WorkSpawner` methods | Phase 1 |
| 3 | Reconcile sweep extension: detect stuck Triaged / Accepted / InProgress with `updated_at`-keyed grace window | Phase 2 |
| 4 | Pattern tracker: `state_hash`, `ActionFingerprint`, `DirectorPatternTracker` struct + thresholds | - |
| 5 | `DirectorMode` enum + transition table + system prompt mode-aware section | Phase 4 |
| 6 | User-prompt mode label + `DirectorState` / `DirectorIterCtx` plumbing | Phase 5 |
| 7 | `OperatorNote` domain record + `NotesStore` (JSONL-backed) | - |
| 8 | IPC verb `DirectorChat` + CLI verb `loopr director chat` + daemon-side handler | Phase 7 |
| 9 | Director loop integration of operator notes + per-Plan `operator_notify` `Arc<Notify>` map | Phase 8 |
| 10 | `NeedsOperator -> Stalled` escalation timeout (mode persists N iterations without notes -> Plan Stalled) | Phases 5, 9 |
| 11 | Telemetry sweep, CLAUDE.md updates, design-doc -> Implemented, bump | All |

Phase boundaries are deliberate: each is small enough to land in one commit with `otto ci` green, and each has an isolatable test surface. Phases 4-6 land independently of Phases 7-9 — modes work without operator chat (just escalate to Stalled when `NeedsOperator` times out); chat works without modes (just renders into history). Wiring them together (Phase 10) is its own phase.

### Architecture

```
crates/agents/src/director.rs
  pub trait WorkSpawner {                                        // EXTENDED
      // Phase 1 (existing):
      fn accept_bundle(&self, bundle_id: BundleId);
      fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String);
      fn assign_work(&self, work_id: WorkId);
      // Phase 2 additions:
      fn spawn_reviewer(&self, bundle_id: BundleId);
      fn spawn_integrator(&self, bundle_id: BundleId);
      fn list_running_work_ids(&self) -> Vec<WorkId>;
      fn list_running_reviewer_bundle_ids(&self) -> Vec<BundleId>;
      fn list_running_integrator_bundle_ids(&self) -> Vec<BundleId>;
  }

  pub enum DirectorMode { Normal, Conservative, NeedsOperator }   // NEW (Phase 5)

  pub struct DirectorPatternTracker { ... }                       // NEW (Phase 4)

  pub fn reconcile_director(...) -> Result<bool>                  // EXTENDED (Phase 3)
      // Phase 1: Integrated -> Done + GoalComplete check.
      // Phase 3 additions (each guarded by `updated_at`-keyed grace window):
      //   - Triaged Bundle without live Reviewer -> spawn_reviewer.
      //   - Accepted Bundle without live Integrator -> spawn_integrator.
      //   - InProgress Work without live Implementer -> override_work(Ready).

  pub fn run_director_inner(...)                                  // EXTENDED
      // Phase 4-5 additions:
      //   - Pattern tracker observe(action, state_hash) at iteration end.
      //   - Mode transition based on pattern observations.
      // Phase 9 additions:
      //   - Fetch unread OperatorNotes via DirectorStore.
      //   - Mark notes read after appending to context.

crates/domain/src/note.rs                                          // NEW (Phase 7)
  pub struct OperatorNote { id, plan_id, author, message, created_at, read_at }

crates/store/src/notes.rs                                          // NEW (Phase 7)
  pub struct NotesStore;                                           // JSONL, mirrors works.rs

crates/ipc/src/messages.rs                                         // EXTENDED (Phase 8)
  pub enum Method  { ..., DirectorChat { plan_id, message } }
  pub enum Reply   { ..., DirectorChat { note_id } }

crates/loopr/src/transport/handler.rs                              // EXTENDED (Phase 8)
  // handle_director_chat: validate, persist OperatorNote, notify.

crates/loopr/src/cli.rs                                            // EXTENDED (Phase 8)
  // `loopr director chat <plan-id> "<msg>"` subcommand.

crates/loopr/src/daemon/context.rs                                 // EXTENDED
  // Phase 1: implementer_work_ids / reviewer_bundle_ids / integrator_bundle_ids
  //          Arc<RwLock<HashMap>> sidecar maps; spawn wrapper RAII guards.
  // Phase 9: operator_notifies: Arc<RwLock<HashMap<PlanId, Arc<Notify>>>>;
  //          per-Plan Notify lifecycle owned by DaemonContext.

crates/context/prompts/agents/director/system.pmt                  // EXTENDED (Phase 5)
  // Mode-aware recovery section listing guidance per mode. Byte-stable.

crates/context/prompts/agents/director/user.pmt                    // EXTENDED (Phase 6)
  // **Director mode:** {mode} label.
  // ## Operator Notes section (Phase 9).
```

### Stuck-State Detection (Phases 1-3)

#### Triaged Bundle without live Reviewer

A Bundle becomes `Triaged` when `propose_bundle` completes. The daemon's `spawn_reviewer_for_bundle` then transitions `Triaged -> Reviewed` once the Reviewer's verdict arrives. If the daemon crashes between `Triaged` persist and the Reviewer task body's first poll, or the Reviewer task panics before transitioning, the Bundle stays `Triaged` indefinitely.

Detection (Phase 3 reconcile-sweep extension):

```rust
let now_ms = now_millis();
let grace_ms = (deps.config.reconcile_grace_secs as i64) * 1000;
let live_reviewer_bundles: HashSet<_> = spawner
    .list_running_reviewer_bundle_ids()
    .into_iter()
    .collect();
for b in bundles.iter().filter(|b| b.status == BundleStatus::Triaged) {
    if (now_ms - b.updated_at) < grace_ms { continue; }   // grace window
    if !live_reviewer_bundles.contains(&b.id) {
        warn!(bundle_id = %b.id, age_ms = now_ms - b.updated_at, "reconcile: Triaged Bundle with no live Reviewer; re-spawning");
        spawner.spawn_reviewer(b.id.clone());
        recovered += 1;
    }
}
```

Re-spawn semantics: `spawn_reviewer` lands a fresh task in the existing `reviewer_tasks` JoinSet. The Bundle's FSM state is not modified by this call; the new Reviewer's verdict path drives `Triaged -> Reviewed` as normal. If the original Reviewer was zombie (live JoinHandle but stuck), Phase 1's sidecar map prevents double-spawn — `list_running_reviewer_bundle_ids` returns the alive set, so the live-zombie Reviewer is reported as live and skipped.

#### Accepted Bundle without live Integrator

Bundle enters `Accepted` via `WorkSpawner::accept_bundle` (Director's path). The current impl (`crates/loopr/src/daemon/context.rs:1253-1257`) early-returns on already-Accepted without spawning the Integrator:

```rust
match bundle.status {
    BundleStatus::Accepted => {
        debug!(bundle_id = %bundle_id, "accept_bundle: already Accepted; no-op");
        return;     // <-- no Integrator spawn
    }
    BundleStatus::Reviewed => {}    // proceed to spawn
    other => { /* warn+skip */ }
}
```

That's the right behavior for "Director re-emitted AcceptBundle by mistake" — but it means the same call cannot recover an Accepted Bundle whose Integrator never spawned (or panicked before the FSM landed `Accepted -> Integrating`). Phase 2 introduces a separate `spawn_integrator(bundle_id)` method that always spawns, guarded by a status check:

```rust
fn spawn_integrator(&self, bundle_id: BundleId) {
    // body re-reads Bundle, requires status == Accepted, spawns into integrator_tasks.
}
```

Detection (Phase 3):

```rust
let live_integrator_bundles: HashSet<_> = spawner
    .list_running_integrator_bundle_ids()
    .into_iter()
    .collect();
for b in bundles.iter().filter(|b| b.status == BundleStatus::Accepted) {
    if (now_ms - b.updated_at) < grace_ms { continue; }
    if !live_integrator_bundles.contains(&b.id) {
        warn!(bundle_id = %b.id, age_ms = now_ms - b.updated_at, "reconcile: Accepted Bundle with no live Integrator; spawning");
        spawner.spawn_integrator(b.id.clone());
        recovered += 1;
    }
}
```

#### InProgress Work without live Implementer

A Work enters `InProgress` from `Ready` once the daemon's dep-gate dispatch fires. If the Implementer panics before the FSM cleanup path (it normally transitions to `InReview` on `propose_bundle`, or `Blocked` on lifeguard escalation), the Work stays `InProgress` with no live worker.

Detection:

```rust
let live_work_ids: HashSet<_> = spawner.list_running_work_ids().into_iter().collect();
for w in works.iter().filter(|w| w.status == WorkStatus::InProgress) {
    if (now_ms - w.updated_at) < grace_ms { continue; }
    if !live_work_ids.contains(&w.id) {
        warn!(
            work_id = %w.id,
            attempt_count = w.attempt_count,
            age_ms = now_ms - w.updated_at,
            "reconcile: InProgress Work with no live Implementer; transitioning to Ready"
        );
        spawner.override_work(
            w.id.clone(),
            WorkStatus::Ready,
            "reconcile: InProgress with no live Implementer".into(),
        );
        recovered += 1;
    }
}
```

The recovery is `override_work(Ready)`, which goes through the same Layer-1 increment site as Director-emitted retries (Phase 1 follow-ups). The retry budget naturally caps repeated panics: a Work that panics three times cycles `Ready -> InProgress -> (panic) -> Ready` until `attempt_count` hits the cap, at which point Layer 2 transitions the Plan to `Stalled`.

Note: the `override_work(Ready)` recovery does NOT itself spawn an Implementer. It transitions the Work to Ready and persists. The daemon's dep-gate watcher promotes Ready -> InProgress reactively and spawns the Implementer; that path was already wired in Phase 1.1 (dep gate). If observation shows the dep-gate watcher is not firing on override-driven Ready states, Phase 3's recovery additionally calls `spawner.assign_work(work_id)` after the override (idempotent — `assign_work` checks the dep gate and no-ops if not Ready). For now we trust the dep gate; the integration test in Phase 3 will fail if it doesn't fire.

#### `WorkSpawner::list_running_*_ids` semantics

The daemon already tracks `implementer_tasks: Mutex<JoinSet<()>>`, `reviewer_tasks: Mutex<JoinSet<()>>`, `integrator_tasks: Mutex<JoinSet<()>>`. The list helpers must expose the *Work ID* / *Bundle ID* each running task owns, not just JoinSet length. Phase 1 adds three sidecar maps:

```rust
pub implementer_work_ids: Arc<RwLock<HashMap<WorkId, ()>>>,
pub reviewer_bundle_ids: Arc<RwLock<HashMap<BundleId, ()>>>,
pub integrator_bundle_ids: Arc<RwLock<HashMap<BundleId, ()>>>,
```

The map values are `()` because we only need the keys. (Storing `AbortHandle` is tempting for forced shutdown, but the existing JoinSet already provides `abort_all`; the map is just a presence index.) The existing spawn wrappers (`spawn_implementer_for_work`, `spawn_reviewer_for_bundle`, `spawn_integrator_for_bundle`) get a tiny RAII helper that inserts on entry and removes on `Drop`:

```rust
struct ScopedIdGuard<K: Hash + Eq> {
    map: Arc<RwLock<HashMap<K, ()>>>,
    key: K,
}
impl<K: Hash + Eq> Drop for ScopedIdGuard<K> {
    fn drop(&mut self) {
        // Use blocking write() so cleanup is guaranteed; the lock is held
        // for one HashMap::remove and never crosses an .await.
        if let Ok(mut g) = self.map.write() {
            g.remove(&self.key);
        }
    }
}
```

The guard owns a cloned `Arc` (no borrow lifetime) so it satisfies tokio's `'static` bound when the spawn-wrapper moves it into a `JoinSet::spawn` body. `Drop` fires on every exit, panic or success.

The list helper is sync (`WorkSpawner` trait contract) and uses **blocking** `read()`, NOT `try_read()`:

```rust
fn list_running_work_ids(&self) -> Vec<WorkId> {
    self.0.implementer_work_ids
        .read()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()  // poisoned only; never empty due to contention
}
```

`try_read` was the original draft; rejected because a single failed `try_read` (during a microsecond write-lock window for an insert or remove) would return an empty `Vec`, and the reconcile sweep would interpret that as "no live tasks" and re-spawn EVERY past-grace Triaged Bundle, Accepted Bundle, and InProgress Work in the Plan. Mass-respawn from one transient lock contention is the failure mode. Blocking `read()` waits the microsecond instead. Reads under the Director's seconds-apart cadence have effectively zero contention; the only failure mode is poison (mutex held across a panicking writer), which `unwrap_or_default()` degrades to empty — same hazard as `try_read`, but only on actual panic, not on routine contention. Phase 1 test asserts the list helper returns the full live set under contention by spawning 100 wrappers concurrently and asserting the list is exactly 100 entries.

### Pattern Tracker (Phase 4)

The tracker observes the Director's emitted actions and the Plan's state hash across iterations. It lives on the Director task, in-memory, reset on restart.

```rust
pub struct DirectorPatternTracker {
    action_history: VecDeque<ActionFingerprint>,    // bounded ring (last `window`)
    state_hash_history: VecDeque<u64>,              // bounded ring (last 8)
    config: PatternConfig,
}

#[derive(PartialEq, Eq, Hash, Clone)]
struct ActionFingerprint {
    kind: &'static str,                  // "accept_bundle", "override_work", etc.
    target_id: String,                   // bundle_id or work_id; "" for Done/NeedHelp
    target_status: Option<String>,       // Some("Ready") for OverrideWork, None otherwise
}

pub struct PatternConfig {
    pub same_action_threshold: u32,    // default: 3   (consecutive identical actions)
    pub no_progress_threshold: u32,    // default: 5   (consecutive iterations same state_hash)
    pub escalation_threshold: u32,     // default: 8   (escalate to NeedsOperator)
    pub window: usize,                 // default: 16  (bounded ring depth)
}
```

#### `state_hash`: what's in, what's out

```rust
fn compute_state_hash(works: &[Work], bundles: &[Bundle]) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut work_tuples: Vec<_> = works.iter()
        .map(|w| (w.id.to_string(), w.status))   // attempt_count INTENTIONALLY EXCLUDED
        .collect();
    work_tuples.sort_by(|a, b| a.0.cmp(&b.0));
    work_tuples.hash(&mut hasher);
    let mut bundle_tuples: Vec<_> = bundles.iter()
        .map(|b| (b.id.to_string(), b.status))
        .collect();
    bundle_tuples.sort_by(|a, b| a.0.cmp(&b.0));
    bundle_tuples.hash(&mut hasher);
    hasher.finish()
}
```

`attempt_count` is **excluded** by design. If it were included, every Director-emitted retry (`Blocked -> Ready`) would bump the count and change the hash, masking a stuck cycle as "progress." With it excluded, a Work cycling `Blocked -> InProgress -> Blocked` produces a small set of distinct hashes (one per status value) that recur over the iteration window — which the recurrence-frequency detector below catches.

Determinism: hashes are stable across daemon restarts because `DefaultHasher` is `SipHasher13` with a fixed seed in std; we don't need cross-process stability, just intra-process determinism. The sorting before hash makes the result order-independent.

#### No-progress detection: recurrence-frequency over the hash window with action-context gate

Architect Round 1 caught that a consecutive-identical-hash check fails to detect cycles: a Work bouncing `Blocked <-> InProgress` produces alternating hashes `H_1, H_2, H_1, H_2, ...` that never sit consecutively identical. Architect Round 2 graded three replacement shapes; the agreed design is recurrence-frequency over a windowed history, gated by Director-action context.

**Trip condition (NoProgressTripped):** the tracker fires when, over the last `window` iterations:

```
trip_no_progress =
    (   distinct_count(hashes) <= 2
     OR max_recurrence(hashes) >= (window / 2) + 1   )
AND any_mutating_action_in_window
```

- `distinct_count <= 2` catches static state (1 distinct) and pure 2-cycles (`Blocked <-> InProgress`).
- `max_recurrence >= (window / 2) + 1` (e.g. 5 of 8) catches longer cycles dominated by a "gravity" state — chaotic 3-cycles like `A, B, C, A, B, C, A, B` slip past the AND form of these clauses (`distinct = 3, recurrence = 3`), which is why the rule is OR.
- The **action-context gate** (`any_mutating_action_in_window`) is the critical mitigation. A long-running Implementer that takes 10 iterations to produce a Bundle leaves the Plan-level hash unchanged for 10 iterations — `distinct_count = 1` would trip without the gate, falsely escalating a healthy in-progress run. A mutating action is any `DirectorAction` variant that changes FSM state: `AcceptBundle`, `OverrideWork`, `AssignWork`, plus the Phase 2 additions when emitted (currently the LLM emits these via the existing variants). Passive `Done` actions and `NeedHelp` do not count. If the Director's last `window` iterations contain only `Done`, the Plan is waiting, not stuck.

Per-window state on the tracker:

```rust
fn observe(&mut self, action: ActionFingerprint, state_hash: u64) -> Option<PatternObservation> {
    push_bounded(&mut self.action_history, action.clone(), self.config.window);
    push_bounded(&mut self.state_hash_history, state_hash, self.config.window);

    if self.state_hash_history.len() < self.config.no_progress_threshold as usize {
        return None;  // not enough samples yet
    }

    let mutating = self.action_history.iter().any(|a| is_mutating(a));
    if !mutating {
        return None;  // gate: passive iterations do not trip
    }

    let distinct = distinct_count(&self.state_hash_history);
    let max_rec = max_recurrence(&self.state_hash_history);
    let half_plus_one = (self.config.window / 2) + 1;

    if distinct <= 2 || max_rec >= half_plus_one {
        return Some(PatternObservation::NoProgressTripped {
            distinct,
            max_recurrence: max_rec,
        });
    }

    // existing same-action / recovery checks below ...
}

fn is_mutating(a: &ActionFingerprint) -> bool {
    matches!(a.kind, "accept_bundle" | "override_work" | "assign_work")
    // Phase 2 may extend with director-emitted spawn variants if added to
    // DirectorAction; "done" and "need_help" are intentionally non-mutating.
}
```

`PatternObservation::NoProgressTripped` carries `distinct` and `max_recurrence` so the `mode_change` event records which clause fired (operator can grep for "stuck via 2-cycle" vs "stuck via gravity-state recurrence").

**Why this design beats the rejected alternatives.**
- Per-Work cycle detector (rejected by Architect): redundant with Phase 1 follow-ups' Layer-2 `attempt_count` cap. Single-Work FSM cycles are already caught when `attempt_count >= max_work_attempts` (default 3) — Plan transitions to Stalled. The pattern tracker's mandate is *cross-Work* macro-pathologies, which require whole-Plan visibility.
- Forward-progress score on a partial order (rejected by Architect): falsely trips on long-running Implementer (score is static while a single Work runs); incoherent rank for `Blocked` (off-ramp, not sequential rank). The recurrence-frequency + action-context combination handles both cases without needing a partial order.

#### Observation logic

`tracker.observe(action_fingerprint, state_hash) -> Option<PatternObservation>` is called at iteration end (after actions execute, before sleep). It returns:

```rust
pub enum PatternObservation {
    SameActionTripped { kind: &'static str, count: u32 },
    NoProgressTripped { count: u32 },
    EscalationTripped { reason: &'static str },
    Recovered,                                         // hash changed AND action variety
    None,                                              // nothing notable
}
```

Recovery rule: `Recovered` fires when the latest state_hash differs from the previous AND the most recent action does NOT match the most recent same-action repeat candidate. Both conditions are required.

### Escalation Modes (Phase 5)

Three modes:

```rust
pub enum DirectorMode {
    /// Default. Standard prompt, standard cap.
    Normal,
    /// Pattern tracker fired same_action or no_progress threshold. Prompt
    /// instructs the LLM to prefer Done over OverrideWork; emit NeedHelp
    /// sooner; cap is unchanged but the prompt nudge biases toward pause.
    Conservative,
    /// Pattern tracker fired escalation_threshold. Prompt instructs the
    /// LLM to emit need_help unless an operator note arrived this iteration.
    /// If NeedsOperator persists `needs_operator_grace_iters` iterations
    /// without an operator note (Phase 10), Director transitions Plan to
    /// Stalled.
    NeedsOperator,
}
```

Mode is a Director task-local field; it does NOT persist on the Plan record. A Director restart reverts to `Normal`; the pattern tracker re-observes from the fresh history.

#### System-prompt cache locality

Per `agents/CLAUDE.md`, the system prompt must be byte-stable across iterations to keep the Anthropic ephemeral-cache hit rate high. Mode-aware behavior cannot vary the system prompt per iteration. The chosen approach:

- The system prompt has a single fixed `## Mode-Aware Recovery` section listing guidance for each mode (~200 tokens of fixed text).
- The user prompt's state summary includes a `**Director mode:** Conservative` label near the top.
- The LLM reads the label and applies the matching guidance block. System prompt stays byte-stable.

Alternative-considered: per-mode system prompts (Option B). Cache misses on every mode transition; for a Director that bounces `Normal -> Conservative -> Normal` under load, this wrecks the hit rate. Rejected.

#### Mode transition table

```rust
fn next_mode(current: DirectorMode, observation: &PatternObservation) -> DirectorMode {
    use DirectorMode::*;
    use PatternObservation::*;
    match (current, observation) {
        // Normal -> Conservative on first sign of trouble.
        (Normal, SameActionTripped { .. })            => Conservative,
        (Normal, NoProgressTripped { .. })            => Conservative,
        // Conservative -> NeedsOperator on sustained trouble.
        (Conservative, EscalationTripped { .. })      => NeedsOperator,
        (Conservative, NoProgressTripped { count }) if *count >= /* escalation_threshold */
                                                      => NeedsOperator,
        // Recovery: any mode -> Normal when state hash changes AND action variety returns.
        (_, Recovered)                                => Normal,
        // Operator engagement is itself a recovery signal. Both Conservative
        // and NeedsOperator demote to Normal on note arrival; Normal stays
        // Normal (idempotent). Architect Round 1 catch: previously only
        // NeedsOperator handled this, leaving Conservative stuck waiting
        // for organic Recovered despite active operator intervention.
        (_, OperatorNoteArrived)                      => Normal,
        // Otherwise, mode is sticky.
        (mode, _)                                     => mode,
    }
}
```

`Recovered` is the canonical demotion path: the Director observes that the system has unstuck (hash moved, action variety returned). The Phase 4 tracker emits this when both conditions hold. NO time-based demotion - no auto-revert without a positive recovery signal. This explicitly answers the "Conservative -> Normal criterion" question (previously buried in a risk row): the criterion is `PatternObservation::Recovered` from the tracker, computed from `state_hash` change AND action-fingerprint variety.

A `mode_change` event fires on every transition. Telemetry: `director.mode_change` info-level event with `from`, `to`, `trigger` fields.

### User-Intervention Chat (Phases 7-9)

Operators send messages that route into the Director's history. Persistence + IPC + CLI + Director loop integration.

#### `OperatorNote` record (Phase 7)

A new domain record persisted alongside Plans/Works/Bundles. JSONL-backed in TaskStore.

```rust
pub struct OperatorNote {
    pub id: NoteId,                              // typed ID
    pub plan_id: PlanId,                         // foreign key
    pub author: String,                          // env USER at submission time
    pub message: String,                         // operator-supplied; capped at 4 KB
    pub created_at: chrono::DateTime<Utc>,
    pub read_at: Option<chrono::DateTime<Utc>>,  // set when Director ingests it
}
```

`NotesStore` mirrors `WorksStore` / `BundlesStore`: JSONL append-only at `<plan-dir>/operator-notes.jsonl`; SQLite index keyed by `(plan_id, read_at IS NULL)` for fast unread-list queries. Methods: `create`, `list_unread_for_plan`, `mark_read(ids: &[NoteId], ts)`.

**`mark_read` write semantics.** Following the `WorksStore::update` OCC pattern: each `mark_read` call appends a fresh full-record line to JSONL with `read_at = Some(ts)` (NOT a separate "read state" file, NOT in-place mutation). The SQLite index updates atomically alongside the append, so JSONL remains the canonical record and the index never diverges. Replaying JSONL from scratch reconstructs the latest `read_at` value (last-write-wins on `id`). Unlike Works/Bundles, OperatorNote has no FSM transitions and no `expected_updated_at` OCC field — `mark_read` writes are blind appends because there is no concurrent writer (only one Director per Plan, only one IPC handler creates notes, and `read_at` is monotonic None -> Some). If a future feature lands concurrent writers (e.g. two Directors per Plan, multi-author note threads), revisit and add OCC.

**Why blind appends are safe at the JSONL line level.** JSONL line-atomicity (no partial writes, no interleaving across concurrent appenders to the same file) is NOT a property of POSIX append semantics in general — it is provided by the underlying `taskstore` library's per-file write dispatcher, which serializes all writes to a given JSONL through a single tokio task. `NotesStore` inherits that guarantee by going through the same dispatcher; nothing in the Phase 2 design adds a side-channel write path that could race the dispatcher. Any future change to `NotesStore` that bypasses the dispatcher must add explicit locking to preserve line-atomicity.

#### IPC verb (Phase 8)

```rust
// crates/ipc/src/messages.rs
pub enum Method {
    ...,
    DirectorChat { plan_id: PlanId, message: String },
}
pub enum Reply {
    ...,
    DirectorChat { note_id: NoteId },
}
```

`handle_director_chat` in `crates/loopr/src/transport/handler.rs`:
1. Validates `plan_id` exists.
2. Validates `message.len() <= 4096`; truncates with marker if longer.
3. Creates an `OperatorNote`; persists via `NotesStore::create`.
4. Notifies the Director task (per-Plan `Arc<Notify>` lookup; details below).
5. Returns `Reply::DirectorChat { note_id }`.

#### CLI verb (Phase 8)

```
loopr director chat <plan-id> "<message>"
```

CLI subcommand under a new `director` namespace. `director` becomes a multi-subcommand parent reserving the namespace for future verbs (status, clear-stalled, etc.). Phase 8 ships `chat` only.

#### `operator_notify` ownership and lifecycle (Phase 9)

The IPC handler's wakeup target must survive Director restarts. Owner: `DaemonContext`.

```rust
// crates/loopr/src/daemon/context.rs
pub operator_notifies: Arc<RwLock<HashMap<PlanId, Arc<Notify>>>>,
```

Lifecycle:

1. **Plan creation.** `handle_plan_create` (after Director task spawn): `ctx.operator_notifies.write()?.insert(plan_id.clone(), Arc::new(Notify::new()));`.
2. **Director restart.** When `run_director` restarts (Phase 1's outer loop, max 3 times), it re-reads `DirectorDeps` which holds the same `Arc<Notify>` cloned from the map. Map entry persists; Director sees the same Notify across restarts.
3. **Daemon restart.** `startup_reconcile_directors` (added in Phase 1's design) iterates Active Plans. For each, it inserts a fresh `Arc<Notify>` into `operator_notifies` BEFORE spawning the Director. Operator notes that arrived during the daemon's downtime were persisted to `NotesStore`; the new Director's first iteration reads them via `list_unread_for_plan`. No Notify wakeup needed for backlog.
4. **Plan terminal.** `transition_and_persist_plan` removes the entry from `operator_notifies` when the Plan reaches a terminal status. Operator notes can still be persisted (the row exists), but the wakeup is a no-op (no live Director).
5. **Stalled -> Active reactivation.** When the operator runs `loopr plan override <plan-id> --to active` to revive a Stalled Plan, `transition_and_persist_plan` observes the `Stalled -> Active` override and inserts a fresh `Arc<Notify>` into `operator_notifies` BEFORE the daemon's reconcile path respawns the Director. This is symmetric with the terminal-removal step above: `transition_and_persist_plan` is the single chokepoint that owns map insert/remove on Plan FSM transitions. The override path therefore needs no additional wiring.

`DirectorDeps` grows:

```rust
pub struct DirectorDeps<L, S, C, P> {
    // ... Phase 1 fields ...
    pub operator_notify: Arc<Notify>,    // cloned from DaemonContext.operator_notifies on spawn
}
```

#### Director loop integration (Phase 9)

At the top of each iteration (before `build_director_state`), the Director:

1. Calls `deps.store.list_unread_notes_for_plan(plan_id)`.
2. If non-empty, the tracker receives a `OperatorNoteArrived` signal; mode transitions to `Normal` if currently `NeedsOperator`.
3. Renders the notes into the user prompt's `## Operator Notes` section (top 8 by `created_at`, oldest first; `+N more` marker if exceeded).
4. After the LLM call returns successfully, calls `deps.store.mark_notes_read(&note_ids, Utc::now())`.

Mark-read happens after rendering, unconditionally on the LLM call's success. If the LLM call fails (transient), the notes stay unread and will be re-rendered on retry. The intent: each note is consumed exactly once by a successful LLM call. Cost is one duplicate render across a transient retry, which is acceptable.

Notes appear ONCE in the LLM context (the iteration they arrive). Subsequent iterations don't re-include them - they live in cross-iteration history as part of that turn's user message and survive `trim_history` until aged out.

#### Notify wakeup (Phase 9)

The Director's poll loop currently uses `tokio::select!` against `tokio::time::sleep(...)` and `deps.shutdown.notified()`. Phase 9 adds a third arm:

```rust
tokio::select! {
    _ = tokio::time::sleep(Duration::from_secs(secs)) => {}
    _ = deps.shutdown.notified() => return Ok(()),
    _ = deps.operator_notify.notified() => {}     // NEW: skip sleep, run iteration immediately
}
```

`handle_director_chat` calls `notify_one()` on the per-Plan Notify after persisting. This guarantees latency: an operator note arriving during a 60s idle sleep fires the next iteration within milliseconds.

### `NeedsOperator -> Stalled` Escalation (Phase 10)

When the Director enters `NeedsOperator`, it begins counting iterations without operator notes. If the count exceeds `needs_operator_grace_iters` (default 5) and no note has arrived, the Director:

1. Reads the Plan, transitions `Active -> Stalled` (Director role; Phase 1 follow-ups added this transition).
2. Persists the Plan.
3. Returns `DirectorError::NeedHelp` with reason `"NeedsOperator timeout: {N} iterations without operator note"`.

This re-uses the Phase 1 follow-ups' Stalled marker and cold-boot-loop fix: the daemon's `startup_reconcile_directors` filter excludes Stalled Plans.

```rust
// crates/agents/src/director.rs additions to run_director_inner
let mode = ...;  // current mode
if mode == DirectorMode::NeedsOperator {
    needs_operator_iters += 1;
    if !operator_notes_arrived_this_iter && needs_operator_iters >= deps.config.needs_operator_grace_iters {
        // Transition Plan to Stalled, then NeedHelp.
        let mut plan = deps.store.get_plan(plan_id).await?;
        plan.transition(PlanStatus::Stalled, Role::Director)
            .map_err(|e| DirectorError::Fsm(e.to_string()))?;
        deps.store.update_plan(plan).await?;
        return Err(DirectorError::NeedHelp(format!(
            "NeedsOperator timeout: {} iterations without operator note",
            needs_operator_iters
        )));
    }
} else {
    needs_operator_iters = 0;
}
```

Operator-recovery path: same as Phase 1 follow-ups - operator runs `loopr plan override <plan-id> --to active` to clear the Stalled state.

### Data Model

```rust
// crates/agents/src/director.rs additions

pub trait WorkSpawner: Send + Sync + 'static {
    // Phase 1 unchanged.
    fn accept_bundle(&self, bundle_id: BundleId);
    fn override_work(&self, work_id: WorkId, target_status: WorkStatus, reason: String);
    fn assign_work(&self, work_id: WorkId);
    // Phase 2 additions:
    fn spawn_reviewer(&self, bundle_id: BundleId);
    fn spawn_integrator(&self, bundle_id: BundleId);
    fn list_running_work_ids(&self) -> Vec<WorkId>;
    fn list_running_reviewer_bundle_ids(&self) -> Vec<BundleId>;
    fn list_running_integrator_bundle_ids(&self) -> Vec<BundleId>;
}

#[trait_variant::make(Send)]
pub trait DirectorStore: Send + Sync + 'static {
    // Phase 1 unchanged.
    async fn list_works_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Work>, StoreError>;
    async fn list_bundles_for_plan(&self, plan_id: &PlanId) -> Result<Vec<Bundle>, StoreError>;
    async fn get_work(&self, work_id: &WorkId) -> Result<Work, StoreError>;
    async fn get_plan(&self, plan_id: &PlanId) -> Result<Plan, StoreError>;
    async fn update_plan(&self, plan: Plan) -> Result<(), StoreError>;
    // Phase 9 additions:
    async fn list_unread_notes_for_plan(&self, plan_id: &PlanId) -> Result<Vec<OperatorNote>, StoreError>;
    async fn mark_notes_read(&self, ids: &[NoteId], ts: DateTime<Utc>) -> Result<(), StoreError>;
}

pub struct DirectorDeps<L, S, C, P> {
    pub llm: L,
    pub store: S,
    pub context: C,
    pub spawner: P,
    pub config: DirectorConfig,
    pub shutdown: Arc<Notify>,
    pub operator_notify: Arc<Notify>,    // NEW (Phase 9)
}
```

```rust
// crates/agents/src/config.rs additions to DirectorConfig

pub struct DirectorConfig {
    // ... Phase 1 + Phase 1 follow-ups fields ...
    pub reconcile_grace_secs: u64,             // NEW (Phase 3); default: 30
    pub patterns: PatternConfig,               // NEW (Phase 4)
    pub needs_operator_grace_iters: u32,       // NEW (Phase 10); default: 5
}

pub struct PatternConfig {
    pub same_action_threshold: u32,            // default: 3
    pub no_progress_threshold: u32,            // default: 5
    pub escalation_threshold: u32,             // default: 8
    pub window: usize,                         // default: 16
}
```

```rust
// crates/loopr/src/daemon/context.rs additions

pub struct DaemonContext<L: ...> {
    // ... existing fields ...
    pub implementer_work_ids: Arc<RwLock<HashMap<WorkId, ()>>>,         // Phase 1
    pub reviewer_bundle_ids: Arc<RwLock<HashMap<BundleId, ()>>>,        // Phase 1
    pub integrator_bundle_ids: Arc<RwLock<HashMap<BundleId, ()>>>,      // Phase 1
    pub operator_notifies: Arc<RwLock<HashMap<PlanId, Arc<Notify>>>>,   // Phase 9
}
```

### Implementation Plan

#### Phase 1: WorkSpawner trait extension + sidecar maps + RAII guards

**Model:** sonnet

Mechanical: trait method additions, struct fields, RAII helper.

- Add the five new method signatures to `WorkSpawner` in `crates/agents/src/director.rs`.
- Add `implementer_work_ids` / `reviewer_bundle_ids` / `integrator_bundle_ids` `Arc<RwLock<HashMap<..., ()>>>` fields to `DaemonContext`. Initialize in `new()`.
- Define a `ScopedIdGuard<K: Hash + Eq + Clone>` RAII helper in `crates/loopr/src/daemon/context.rs` that takes `(map, key)` on construction and removes on `Drop`.
- Update existing `spawn_implementer_for_work`, `spawn_reviewer_for_bundle`, `spawn_integrator_for_bundle` wrappers: insert into the matching map at the top of the spawned task body; bind the guard so removal fires on every exit (panic or success).
- Update existing `WorkSpawner` impls in test fakes (`crates/agents/src/director/tests.rs`, `crates/loopr/tests/director_reconcile.rs`, etc.) to satisfy the wider trait. Default new methods: empty `Vec` for list helpers; no-op for `spawn_reviewer` / `spawn_integrator`. Tests that need real behavior populate explicitly.
- Tests:
  - Unit (`crates/loopr/src/daemon/context/tests.rs` or similar): spawn-wrapper inserts into map; map is empty after task body returns; map is empty after task body panics.
  - `otto ci` green.

#### Phase 2: DaemonSpawner impl of new WorkSpawner methods

**Model:** sonnet

- Implement `spawn_reviewer` on `DaemonSpawner`: same shim pattern as Phase 1 follow-ups' `accept_bundle` (sync trait -> async lock bridge via inner `tokio::spawn`). Body: validates Bundle status (must be `Triaged`; `Reviewed` and onward are no-ops because re-running the Reviewer would redo work), spawns into `reviewer_tasks` JoinSet via the existing `spawn_reviewer_for_bundle` helper.
- Implement `spawn_integrator` on `DaemonSpawner`: similar; body validates Bundle status (must be `Accepted`), spawns into `integrator_tasks` via `spawn_integrator_for_bundle`.
- Implement the three `list_running_*_ids` methods: sync `try_read` on the matching map, return `Vec<...>` of cloned keys.
- Tests:
  - Unit: `spawn_reviewer` skips for `Reviewed` status (already past Triaged); fires for `Triaged`.
  - Unit: `spawn_integrator` skips for non-Accepted; fires for `Accepted`.
  - Unit: list helpers return current map keys.

#### Phase 3: Reconcile sweep extension (stuck-state detection)

**Model:** opus

Non-mechanical: order of checks, grace-window math, integration with Phase 1's `Integrated -> Done` step.

- Add `reconcile_grace_secs: u64` (default 30) to `DirectorConfig`. Wire as `agents.director.reconcile-grace-secs` in YAML (kebab-case via existing `serde(rename_all = "kebab-case")` on `DirectorConfig`). Add YAML round-trip test in `crates/agents/src/config/tests.rs` (or wherever the existing `DirectorConfig` tests live) covering the new field.
- Extend `reconcile_director` with three new check loops, in order: (1) Triaged-no-Reviewer, (2) Accepted-no-Integrator, (3) InProgress-no-Implementer. Each guarded by `(now_millis() - record.updated_at) >= grace_ms`.
- Each loop emits a `warn!` with `age_ms` and the recovery action. Aggregated `recovered_count` field on the span.
- Tests:
  - Unit: stuck Triaged (no live Reviewer, age > grace) -> `spawn_reviewer` called.
  - Unit: stuck Triaged within grace window -> no spawn.
  - Unit: Triaged WITH live Reviewer -> no spawn.
  - Unit: stuck Accepted -> `spawn_integrator` called; Accepted within grace window -> no spawn.
  - Unit: stuck InProgress -> `override_work(Ready)` called; InProgress within grace window -> no spawn.
  - Integration (`crates/loopr/tests/director_stuck_states.rs`): kill Implementer mid-run via `abort()`; advance daemon clock past grace; assert reconcile fires `override_work(Ready)`; assert dep-gate watcher re-spawns Implementer; assert Plan reaches GoalComplete.

#### Phase 4: Pattern tracker

**Model:** opus

- Add `DirectorPatternTracker`, `PatternConfig`, `ActionFingerprint`, `PatternObservation` types to `crates/agents/src/director.rs`. Wire `PatternConfig` into `DirectorConfig` as `agents.director.patterns` with kebab-case sub-keys (`same-action-threshold`, `no-progress-threshold`, `escalation-threshold`, `window`). Add YAML round-trip test in `crates/agents/src/config/tests.rs` covering the nested `patterns` table with default + override.
- `compute_state_hash(works: &[Work], bundles: &[Bundle]) -> u64`: stable hash via sorted tuples + `DefaultHasher`. Excludes `attempt_count`.
- `tracker.observe(action_fingerprint, state_hash) -> Option<PatternObservation>`: update history, check thresholds, return observation.
- `Recovered` detection: state_hash differs from previous AND action variety in the last `window` is >= 2.
- Tests:
  - Unit: same `OverrideWork(work-x, Ready)` 3x consecutive -> `SameActionTripped`.
  - Unit: same `OverrideWork` interrupted by `Done` -> counter resets, no trip.
  - Unit: hash determinism (same inputs in different orderings produce same hash).
  - Unit: `attempt_count` increment does NOT change hash.
  - Unit: Recovery (hash changed AND variety) -> `Recovered`.
  - Unit (NoProgressTripped, static-state-with-mutation): hash identical 5x AND mutating actions emitted -> `NoProgressTripped { distinct: 1, max_recurrence: 5 }`.
  - Unit (NoProgressTripped, 2-cycle): hashes alternate `H1, H2, H1, H2, H1, H2, H1, H2` AND mutating actions emitted -> trips because `distinct == 2`.
  - Unit (NoProgressTripped, gravity-state cycle): hashes `A, A, A, B, A, A, C, A` (window=8, max_rec=5, distinct=3) AND mutating actions -> trips because `max_recurrence >= 5`.
  - Unit (NoProgressTripped, chaotic 3-cycle): `A, B, C, A, B, C, A, B` (distinct=3, max_rec=3) AND mutating actions -> trips because `distinct <= 3` is the looser of the two clauses; verify exact threshold semantics. (This is the OR-clause's stress case Architect flagged.)
  - Unit (action-context gate, NEGATIVE): hash identical 8x BUT actions are all `Done` (passive long-running Implementer) -> NO trip. Critical false-positive guard.
  - Unit (action-context gate, NEGATIVE): hashes alternate 2-cycle but actions are all `Done` -> NO trip. Director hasn't tried anything.
  - Unit: `EscalationTripped` fires when `NoProgressTripped` has held continuously past `escalation_threshold` while still meeting the action-context gate.

#### Phase 5: DirectorMode + transition table + system prompt

**Model:** opus

- `DirectorMode` enum and `next_mode` transition function in `director.rs`.
- Update `crates/context/prompts/agents/director/system.pmt`: add `## Mode-Aware Recovery` section listing guidance for each mode. Section is fixed text; LLM reads the user-message label and applies the matching block.
- `mode_change` event with `from`, `to`, `trigger` fields fires on every transition.
- Tests:
  - Unit: full transition matrix (`Normal -> Conservative -> NeedsOperator`; `_ -> Normal` on `Recovered`).
  - Unit: `Conservative -> Normal` requires `Recovered`, NOT just absence-of-trip.
  - Unit: `system.pmt` byte-stable across two iterations with different modes (regression guard for cache-locality). Note: byte-equality is the assertion regardless of token count; the Anthropic ephemeral-cache silently no-ops below 2048 tokens (Sonnet/Opus 4.x), so a below-threshold prompt drift would be invisible at runtime but the test still catches it. If `system.pmt` later grows past the threshold, no test change is needed.

#### Phase 6: User-prompt mode label + DirectorState plumbing

**Model:** sonnet

- Update `crates/context/prompts/agents/director/user.pmt`: add `**Director mode:** {mode}` line near the top.
- Plumb `mode` through `DirectorIterCtx` (a thin wrapper around `DirectorState` if not already present) so `build_for_director` renders it.
- Wire mode into `run_director_inner`: track `current_mode: DirectorMode`, pass to `DirectorIterCtx`, update from `next_mode(current, observation)` at iteration end.
- Tests:
  - Unit: `Director mode: Conservative` appears in rendered user prompt when mode is Conservative.
  - Telemetry test: `director.mode_change` event captured by `events.log` parser.

#### Phase 7: OperatorNote domain record + NotesStore

**Model:** sonnet

- Create `crates/domain/src/note.rs` with `OperatorNote` + `NoteId` types. Add `note` to `lib.rs` exports.
- Create `crates/store/src/notes.rs` with `NotesStore` (mirrors `WorksStore` JSONL pattern). SQLite index by `plan_id` and `read_at`.
- Add `notes()` accessor to `Store`.
- Tests:
  - Unit: create + list_unread + mark_read round-trip.
  - Unit: list_unread filters by `plan_id` (cross-Plan isolation).
  - Unit: list_unread filters by `read_at IS NULL`.

#### Phase 8: IPC verb + CLI verb + handler

**Model:** sonnet

- Add `Method::DirectorChat { plan_id, message }` and `Reply::DirectorChat { note_id }` to `crates/ipc/src/messages.rs`.
- Add `handle_director_chat` to `crates/loopr/src/transport/handler.rs`: validate Plan exists, truncate message at 4096 bytes, create note, persist via `NotesStore::create`. (Notify wakeup arrives in Phase 9.)
- Add `loopr director chat <plan-id> "<message>"` CLI subcommand to `crates/loopr/src/cli.rs`. `director` becomes a multi-subcommand parent.
- Tests:
  - Integration (`crates/loopr/tests/director_chat.rs`): `loopr director chat <plan>` round-trips through daemon, `notes()` shows the message persisted.
  - Unit: 5 KB message -> truncated to 4 KB with marker.
  - Unit: nonexistent plan_id -> error reply.

#### Phase 9: Director loop integration of operator notes + per-Plan Notify

**Model:** opus

Non-mechanical: notify lifecycle, mark-read timing, mode integration.

- Add `operator_notifies: Arc<RwLock<HashMap<PlanId, Arc<Notify>>>>` to `DaemonContext`.
- `handle_plan_create`: insert a fresh `Arc<Notify>` after Director task spawn.
- `startup_reconcile_directors`: for each Active Plan, insert a fresh `Arc<Notify>` BEFORE spawning the Director task. Existing operator notes (persisted across daemon downtime) flow through the first iteration's `list_unread_notes_for_plan` call.
- `transition_and_persist_plan`: on Plan terminal transition, remove the entry.
- `handle_director_chat` (Phase 8 extension): after persisting, look up the Plan's Notify and call `notify_one()`. If the Plan has no Notify entry (terminal Plan), log debug and skip.
- Add `operator_notify: Arc<Notify>` to `DirectorDeps`.
- Update `run_director_inner`:
  - At iteration top: `let unread = deps.store.list_unread_notes_for_plan(plan_id).await?;`
  - If non-empty AND mode == NeedsOperator: signal `OperatorNoteArrived` to tracker; mode -> Normal.
  - Render notes into user prompt's `## Operator Notes` section via `DirectorIterCtx::operator_notes`.
  - After successful LLM call: `deps.store.mark_notes_read(&note_ids, Utc::now()).await?;`
  - Wakeup: third arm in `tokio::select!` against `operator_notify.notified()`.
- Tests:
  - Integration: `loopr director chat` during 60s idle sleep wakes Director within 1s.
  - Integration: chat in `NeedsOperator` mode transitions back to `Normal`.
  - Integration: notes persisted during daemon downtime are read by the post-restart Director's first iteration.
  - Unit: mark_read called only on LLM-success; failure path leaves notes unread.

#### Phase 10: NeedsOperator -> Stalled escalation

**Model:** opus

- Add `needs_operator_grace_iters: u32` (default 5) to `DirectorConfig`. Wire as `agents.director.needs-operator-grace-iters` in YAML. Add YAML round-trip test in `crates/agents/src/config/tests.rs` covering the new field.
- In `run_director_inner`, track `needs_operator_iters: u32` across iterations. Increment when mode == NeedsOperator and no operator note arrived this iteration; reset otherwise.
- When `needs_operator_iters >= needs_operator_grace_iters`: transition Plan -> Stalled (Director role), persist, return `DirectorError::NeedHelp`.
- Tests:
  - Integration: Director enters NeedsOperator, 5 iterations elapse without notes, Plan transitions to Stalled, daemon restart's `startup_reconcile_directors` skips the Plan (no respawn).
  - Integration: NeedsOperator + operator note in iter 3 -> mode reverts to Normal, no Stalled.

#### Phase 11: Telemetry + documentation + rollout

**Model:** sonnet

- Update `crates/agents/CLAUDE.md` "Instrumentation" section: list new spans (`director.mode_change`, `director.operator_note`, `director.reconcile_recovery`).
- Update `crates/loopr/CLAUDE.md` to mention the new `operator_notifies` map and the `loopr director chat` CLI verb.
- Update `docs/telemetry-grep-cookbook.md` with operator grep patterns (e.g. `director.mode_change to=NeedsOperator` for "find Plans currently in escalation").
- Update auto-memory: create `~/.claude/projects/-home-saidler-repos-scottidler-loopr/memory/project-director-phase-2-shipped.md` summarizing what landed and any known follow-ups.
- Mark this design doc `Status: Implemented`.
- Bump (`/bump`), push, install.

---

## Alternatives Considered

### Alternative 1: Item 4 (judgment plane) deferred to Phase 3

- **Description:** Phase 2 covers only items 1-3 (deterministic stuck-state detection). Item 4 lands as a separate "Phase 3: Director judgment plane" design doc.
- **Pros:** Phase 2 ships sooner (mechanical work only). Smaller blast radius per ship.
- **Cons:** Splitting requires its own design doc and a separate ship cycle for what's a logical extension. The four items are coherent as one Phase 2; the phasing inside this doc keeps items 1-3 isolatable (Phases 1-3) so they can ship first if we choose to pause.
- **Why not chosen:** Chosen by user during scope clarification.

### Alternative 2: Persistent operator messages via append-only log file (not TaskStore)

- **Description:** Operator chat messages append to a per-Plan log file (`<plan-dir>/operator-notes.jsonl`); Director reads on each iteration. No domain record, no SQLite index.
- **Pros:** No domain-record changes; no store changes.
- **Cons:** Bypasses the TaskStore anti-corruption layer. No transactional guarantees. Lost on `loopr clean`. Breaks the "all state lives in TaskStore" rule.
- **Why not chosen:** Consistency. TaskStore is the single source of truth.

### Alternative 3: Mode persisted on Plan record (not Director-task-local)

- **Description:** `PlanStatus` grows to include `Active(DirectorMode)` or a parallel `Plan.director_mode` field. Mode survives Director restart.
- **Pros:** Mode isn't lost on restart; pattern tracker observations don't have to re-warm.
- **Cons:** Pollutes Plan with Director-specific state; the Plan FSM table grows non-trivially. Restart frequency (max 3 per Phase 1) is low and the pattern tracker re-warms within 3-5 iterations after restart.
- **Why not chosen:** Cost/benefit favors Director-task-local. Restarts are rare; mode is operational state, not Plan state.

### Alternative 4: Per-mode system prompts (cache-locality sacrifice)

- **Description:** Three `system.pmt` variants, one per mode. Director swaps the system prompt on mode transition.
- **Pros:** Cleaner per-mode prompt structure; LLM doesn't have to read a label + apply matching block.
- **Cons:** Cache misses on every mode transition. Conservative-Normal flapping under load destroys the cache hit rate. Cleaner-prompt savings don't offset the cost.
- **Why not chosen:** Cache locality is load-bearing per `agents/CLAUDE.md`.

### Alternative 5: `WorkSpawner` returns `Result` from new methods (so reconcile observes spawn failures)

- **Description:** `spawn_reviewer` / `spawn_integrator` return `Result<(), SpawnError>`; reconcile escalates on failure.
- **Pros:** Reconcile sweep can decide to escalate to Stalled if recovery fails repeatedly.
- **Cons:** Departs from Phase 1's fire-and-forget contract. Spawn failures are typically only "shutting down" (the body's check handles it) or "ID parse error" (handled inline). Adding a Result return for this rare case complicates every call site.
- **Why not chosen:** Recovery failures will surface as the Bundle/Work staying stuck on the next iteration; the reconcile sweep re-attempts, and if it can't recover, the pattern tracker (Phase 4) catches the cycle and the mode transitions through to Stalled (Phase 10).

### Alternative 6: Sidecar map stores `AbortHandle` instead of `()`

- **Description:** `implementer_work_ids: HashMap<WorkId, AbortHandle>` so the daemon can forcibly abort a stuck Implementer.
- **Pros:** Targeted abort instead of `JoinSet::abort_all`.
- **Cons:** `JoinSet` already provides `abort_all`; per-task abort isn't part of any current need. The map is just a presence index for `list_running_*_ids`.
- **Why not chosen:** YAGNI. Adding the field is trivial later if a use case lands.

---

## Technical Considerations

### Dependencies

No new external crates. All new types live in existing crates; one new file per crate (`note.rs` in `domain`, `notes.rs` in `store`).

### Performance

- Phases 1-3 (sidecar maps + reconcile): three additional store list calls per reconcile, three sidecar map reads. Sub-millisecond at first-gate Plan sizes.
- Phase 4 (pattern tracker): `compute_state_hash` is O(W + B) per iteration with sort; W and B are single-digit at first gate. Negligible.
- Phases 5-6 (modes): zero runtime cost beyond one branch on iteration end.
- Phases 7-9 (operator chat): one extra store call per iteration (`list_unread_notes_for_plan`); empty result is typical, JSONL scan is microseconds.
- Phase 10: zero cost beyond an iteration counter.

Total per-iteration cost rises by ~1-2 ms. Negligible relative to LLM call latency (multi-second).

### Security

- `loopr director chat` writes operator-controlled strings into the LLM prompt. The Director's system prompt should treat operator-note content as untrusted user input. This is no different from the goal text, which is also operator-controlled.
- IPC verb `DirectorChat` truncates the message at 4 KB to bound prompt-injection-by-volume.
- `NoteId` typed wrapper prevents cross-record-kind ID confusion.

### Testing Strategy

Per-phase unit + integration tests above. The convergence test (Phase 11):
1. Start a daemon with a Plan.
2. Kill the Implementer mid-run via `abort()` (simulates panic).
3. Advance grace; assert reconcile fires `override_work(Ready)`.
4. Re-spawned Implementer panics again; loop until pattern tracker fires.
5. Mode transitions through `Normal -> Conservative`.
6. `loopr director chat <plan>` "ignore the flaky tests"; mode reverts to Normal.
7. New Implementer attempt succeeds; Plan completes.

This proves all four items work together end-to-end.

### Rollout Plan

One commit per phase, `otto ci` green between each. After Phase 11 lands, bump (likely v0.7.16 or higher per accumulated patches), push tag.

---

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Reconcile recovery races spawn-chain (Reviewer just spawned, sidecar map insert hasn't completed yet) | Medium | Medium | Phase 3's `reconcile_grace_secs` (30s) on `record.updated_at`. Bundles/Works in their current status less than 30s skip recovery. |
| Sidecar map drift (task panic skips cleanup, map shows phantom IDs) | Medium | Medium | RAII `ScopedIdGuard` removes on every exit, panic or success. Phase 1 test exercises the panicking-task path explicitly. |
| Pattern tracker false positives (a long-running Implementer that legitimately requires multiple `OverrideWork` retries trips `same_action_threshold`) | Medium | Low | Threshold counts CONSECUTIVE iterations only; a single intervening `Done` action resets the counter. Defaults are operator-tunable. The Phase 1 follow-ups' `attempt_count` cap (default 3) bounds the same per-Work retry path independently. |
| Mode flapping (Normal -> Conservative -> Normal -> Conservative within a few iterations) | Low | Low | Mode demotion requires `PatternObservation::Recovered` (state_hash change AND action variety). No time-based auto-revert. |
| Operator chat message volume (operator spams 100 notes during a sleep window) | Low | Low | User prompt section caps render at 8 notes (oldest first); older notes are still marked read; UI shows `+N more`. |
| `OperatorNote` persistence + Director restart: operator wrote a note, Director crashed before reading, restart's pattern tracker still sees pre-note state | Low | Low | Notes persist in TaskStore. Restart's first iteration reads via `list_unread_notes_for_plan`. The note is preserved across restart. |
| `list_running_*_ids` returns stale entries (task body started but not yet inserted) | Low | Low | Insert is the FIRST action in the spawn-wrapper, before any `await`. Removal is via `Drop`. The 30s grace window absorbs the insertion-latency window. |
| `system.pmt` mode-aware section grows on prompt size enough to push history to trim early | Low | Low | Mode-aware section is ~200 tokens (fixed). System prompt budget is generous (>1k tokens reserved). |
| Cache-locality regression: a future contributor adds iteration-specific data to `system.pmt` | Medium | High | Phase 5 test asserts `system.pmt` is byte-stable across two iterations with different modes. CI catches the regression. |
| Cross-Plan operator-note routing bug (note for Plan A appears in Plan B's Director context) | Very Low | High | `list_unread_notes_for_plan` filters by `plan_id`. Phase 7 test asserts isolation explicitly. |
| `operator_notifies` map entry leaked after Plan terminal | Low | Low | `transition_and_persist_plan` removes on terminal. Phase 9 test asserts post-terminal map size. |
| Operator chat to Stalled Plan (Director not running): notify is no-op, note persists silently | Low | Low | Documented behavior. Operator must explicitly `loopr plan override --to active` to reactivate. The chat is preserved and will be read by the next Director's first iteration. |
| Phase 10's NeedsOperator->Stalled fires while a note is in flight (operator just typed but Director hasn't seen it yet) | Low | Medium | The Director's iteration top reads unread notes BEFORE the iteration counter increments; an in-flight note arriving before the read clears the counter via `Recovered` -> Normal. The window is one iteration interval (5-15s). |

---

## Open Questions

- [ ] `PatternConfig` defaults of (3, 5, 8) are placeholders. Tune from observed Stage-9 + first-real-Plan traces. Default-fallback path stays via `DirectorConfig::default()`.
- [ ] `loopr director status <plan-id>` (read-only inspect): Phase 5 or defer? Lean: defer to a follow-up; Phase 8's `chat` proves the round-trip.
- [ ] Operator-note byte cap of 4 KB: confirm or raise to 8 KB? Lean: 4 KB; longer messages should be summarized by the operator.
- [ ] Grace window default of 30s is conservative. If real traces show recoverable stuck states sit much longer (e.g. 5 min Implementer LLM calls), the cost of running the reconcile sweep often is fine, but the cost of recovery firing on a slow Implementer is double-spawn risk. Tune from traces before defaulting.

---

## References

- `docs/design/2026-05-08-director-phase-1.md`: Phase 1 design (deferred items section, lines 476-482; Non-Goals lines 50-58)
- `docs/design/2026-05-09-director-phase-1-followups.md`: Phase 1 follow-ups (Stalled, max_work_attempts, JoinSet drain, retry-budget enforcement)
- `crates/agents/src/director.rs`: Phase 1 shipped surface (run_director, reconcile_director, WorkSpawner, DirectorStore)
- `crates/agents/CLAUDE.md`: cache-locality rule for system prompts
- `crates/loopr/src/daemon/context.rs:1222-1290`: current `DaemonSpawner::accept_bundle` impl (Accepted-no-Integrator early-return at line 1255)
- `crates/loopr/CLAUDE.md`: Shutdown drain order, Retry-budget enforcement, IPC and daemon-startup timeouts
- `crates/store/src/works.rs`, `crates/store/src/bundles.rs`: store pattern Phase 7's `notes.rs` mirrors
- `~/.claude/projects/-home-saidler-repos-scottidler-loopr/memory/project-director-phase-1-shipped.md`: shipped state + known gaps memo
