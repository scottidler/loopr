# Design Document: Loopr v3 — MVP2

**Author:** Scott Aidler
**Date:** 2026-02-26
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

MVP2 adds two capabilities to the MVP1 orchestration spine: (1) durable persistence via TaskStore, replacing in-memory HashMaps so state survives daemon restarts, and (2) a read-only Doc Validator LLM that gates Plan/Spec/Phase quality before they can transition to Active. The Doc Validator is the safest possible entry point for LLM intelligence — it produces structured reports but never modifies state, never touches git, and cannot break Tick semantics.

## Problem Statement

### Background

MVP1 proved the orchestration spine: three FSMs (Work, Bundle, Tick), daemon-mediated correctness, NDJSON IPC over Unix socket, Git worktree management, and a ratatui TUI. All 11 success criteria pass end-to-end. But two gaps remain:

1. **State is ephemeral.** All records live in `HashMap<String, T>` behind `StdRwLock`. When the daemon restarts, everything is gone. The design doc specified TaskStore persistence, but it wasn't in the success criteria and was deferred.

2. **No intelligence.** The human does everything — including judging whether a Spec is complete enough to begin implementation. There's no automated quality gate for the documents that drive all downstream work.

### Problem

- Daemon crash or restart loses all Plans, Specs, Phases, Works, Bundles, Ticks, Learnings, and Locks. This makes loopr unusable for real multi-session work.
- Poor-quality Specs and Plans propagate downstream, creating wasted implementation effort. A human can overlook gaps; a structured validator catches them consistently.
- The TUI receives daemon events but discards them (`tui/run.rs:99` TODO), so the UI doesn't update reactively.

### Goals

- **G1:** All daemon state persists across restarts via TaskStore (JSONL-as-truth, SQLite-as-cache)
- **G2:** `system.init` IPC method creates TaskStore collections and git merge driver
- **G3:** Doc Validator LLM produces structured validation reports for Plan, Spec, and Phase documents
- **G4:** Validation is a prerequisite gate for `Draft → Active` transitions on hierarchy records
- **G5:** TUI applies daemon events to AppState for reactive UI updates
- **G6:** LLM provider is configurable (API key, model, endpoint) via `loopr.yml`

### Non-Goals

- **No LLM-generated content.** The validator reads and judges — it never writes Plans, Specs, or code. That's MVP3+.
- **No async LLM streaming.** The validator is a synchronous gate. Streaming output to TUI is MVP3+.
- **No enforced locks.** Locks remain advisory (soft). Mandatory locking is MVP3+.
- **No multi-user support.** Still single-user, local-only.
- **No policy enforcement from Learnings.** Learning promotion to Policy with automated enforcement is MVP3+.
- **No staleness cascade automation.** When a Tick publishes, in-progress Works are not auto-notified. Human discovers staleness on next bundle proposal (same as MVP1).

## Proposed Solution

### Overview

Two independent pillars that share no coupling:

**Pillar 1: TaskStore Persistence** — Replace `Stores` (8 HashMaps) with a single `taskstore::Store` instance. Every create/update/delete in handlers appends to JSONL and upserts SQLite. On daemon startup, `Store::open()` loads all records from JSONL. Crash recovery runs after load.

**Pillar 2: Doc Validator** — A new `validator` module that calls an LLM API with the content of a Plan, Spec, or Phase plus a structured prompt. Returns a `ValidationReport` (pass/fail, issues, suggestions). The daemon enforces: `Draft → Active` transitions on hierarchy records require a passing validation report. A new `validator.validate` IPC method triggers validation on-demand; the transition handler checks for a stored report.

### Architecture

```
┌──────────────────────────────────────────────────────────┐
│                       TUI / CLI                          │
│   ┌─────────┐   ┌─────────┐   ┌──────────────────────┐  │
│   │ Views   │   │  Input  │   │ Event → AppState     │  │
│   │         │   │         │   │ (reactive updates)   │  │
│   └─────────┘   └─────────┘   └──────────────────────┘  │
└────────────────────┬─────────────────────────────────────┘
                     │ NDJSON / Unix Socket
┌────────────────────▼─────────────────────────────────────┐
│                      Daemon                              │
│  ┌──────────────────────────────────────────────────┐    │
│  │                   Handlers                        │    │
│  │  *.create  *.get  *.list  *.transition            │    │
│  │  validator.validate  system.init                  │    │
│  └─────────┬────────────────────┬────────────────────┘    │
│            │                    │                         │
│  ┌─────────▼──────────┐  ┌─────▼──────────┐             │
│  │   TaskStore         │  │  Doc Validator  │             │
│  │ ┌────────────────┐  │  │ ┌────────────┐ │             │
│  │ │ JSONL (truth)   │  │  │ │ LLM API    │ │             │
│  │ │ SQLite (cache)  │  │  │ │ (read-only)│ │             │
│  │ └────────────────┘  │  │ └────────────┘ │             │
│  └─────────────────────┘  └────────────────┘             │
└──────────────────────────────────────────────────────────┘
```

### Data Model

#### TaskStore Record Trait Implementation

All 8 domain types + `ValidationReport` implement `taskstore::Record`. Implementations live in each domain type's own file (e.g., `domain/plan.rs` gets `impl Record for Plan`). This keeps the `Record` impl next to the struct definition and indexed fields close to the fields they reference.

```rust
use taskstore::{Record, IndexValue};
use std::collections::HashMap;

impl Record for Plan {
    fn id(&self) -> &str { &self.id }
    fn updated_at(&self) -> i64 { self.updated_at }
    fn collection_name() -> &'static str { "plans" }
    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m
    }
}

impl Record for Spec {
    fn id(&self) -> &str { &self.id }
    fn updated_at(&self) -> i64 { self.updated_at }
    fn collection_name() -> &'static str { "specs" }
    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("plan_id".into(), IndexValue::String(self.plan_id.clone()));
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m
    }
}

// Same pattern for Phase (spec_id, status, order), Work (phase_id, status),
// Bundle (work_id, status, base_tick_id), Tick (status, number),
// Learning (source_id, scope), Lock (resource, status)
```

**TaskStore location:** `Store::open()` is called with `config.project.repo_path` (the target repo root). TaskStore creates `.taskstore/` there. This means loopr's records live alongside the target repo's code.

**JSONL file layout** (inside target repo's `.taskstore/`):

```
.taskstore/
├── plans.jsonl         # committed to git
├── specs.jsonl
├── phases.jsonl
├── works.jsonl
├── bundles.jsonl
├── ticks.jsonl
├── learnings.jsonl
├── locks.jsonl
├── validation_reports.jsonl
├── taskstore.db        # gitignored (cache)
└── .gitignore
```

#### ValidationReport

```rust
/// Structured output from the Doc Validator LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub id: String,
    pub target_collection: String,    // "plans", "specs", "phases"
    pub target_id: String,
    pub verdict: ValidationVerdict,
    pub issues: Vec<ValidationIssue>,
    pub summary: String,
    pub model_used: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationVerdict {
    Pass,
    Fail,
    Warn,   // passes but with warnings
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    pub category: String,         // "completeness", "clarity", "testability", "scope"
    pub message: String,
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueSeverity {
    Error,      // blocks activation
    Warning,    // noted but doesn't block
    Info,       // suggestion only
}
```

ValidationReport also implements `Record` for TaskStore persistence:
```rust
impl Record for ValidationReport {
    fn id(&self) -> &str { &self.id }
    fn updated_at(&self) -> i64 { self.created_at }
    fn collection_name() -> &'static str { "validation_reports" }
    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("target_id".into(), IndexValue::String(self.target_id.clone()));
        m.insert("target_collection".into(), IndexValue::String(self.target_collection.clone()));
        m.insert("verdict".into(), IndexValue::String(format!("{:?}", self.verdict).to_lowercase()));
        m
    }
}
```

#### LLM Configuration

Added to `Config`:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ValidatorConfig {
    pub enabled: bool,
    pub provider: String,         // "anthropic", "openai"
    pub model: String,            // "claude-sonnet-4-6"
    pub api_key_env: String,      // env var name, e.g. "ANTHROPIC_API_KEY"
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,             // off by default — opt-in
            provider: "anthropic".into(),
            model: "claude-sonnet-4-6".into(),
            api_key_env: "ANTHROPIC_API_KEY".into(),
            max_tokens: 4096,
            temperature: 0.0,           // deterministic for validation
        }
    }
}
```

### API Design

#### New IPC Methods

| Method | Params | Returns | Role | Description |
|--------|--------|---------|------|-------------|
| `system.init` | `{}` | `{collections: [...]}` | Any | Initialize TaskStore, create collections, install git hooks |
| `validator.validate` | `{collection, id}` | `ValidationReport` | Coordinator | Run Doc Validator on a Plan/Spec/Phase |
| `validator.report` | `{target_id}` | `ValidationReport` | Any | Get latest validation report for a record |
| `validator.reports` | `{target_id?}` | `[ValidationReport]` | Any | List validation reports, optionally filtered |

#### Modified Transitions

The `plan.transition`, `spec.transition`, and `phase.transition` handlers gain a validation gate:

```
Draft → Active:
  1. Check FSM rules (existing)
  2. Check role (existing)
  3. NEW: If validator.enabled, require a ValidationReport with verdict=Pass or verdict=Warn
         for this record. If no report exists or latest report is Fail, reject with error -32003.
```

**Finding the latest report:** The gate queries TaskStore:
```rust
let reports = store.lock().unwrap().list::<ValidationReport>(&[
    Filter { field: "target_id".into(), op: FilterOp::Eq, value: IndexValue::String(id.clone()) },
])?;
// Reports are returned ordered by updated_at DESC — first element is latest
let latest = reports.first();
```

New error code:
```rust
-32003: validation_required — "Draft → Active requires a passing validation report.
         Run 'validator.validate' first."
```

#### Modified Handlers

**Dispatch signature changes:**

```rust
// MVP1
pub fn dispatch(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    integrator_config: &IntegratorConfig,
    req: DaemonRequest,
) -> DaemonResponse

// MVP2
pub fn dispatch(
    store: &Arc<Mutex<taskstore::Store>>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    config: &Config,              // full config (includes integrator + validator)
    req: DaemonRequest,
) -> DaemonResponse
```

Every individual handler function signature changes correspondingly (`&Arc<Stores>` → `&Arc<Mutex<Store>>`).

**CRUD pattern changes.** All `*.create`, `*.transition`, `learning.*`, `lock.*` handlers change from:
```rust
// MVP1: HashMap insert
stores.plans.write().unwrap().insert(id, plan);
```
to:
```rust
// MVP2: TaskStore create/update
store.lock().unwrap().create(plan)?;
// or
store.lock().unwrap().update(plan)?;
```

All `*.get` handlers change from:
```rust
stores.plans.read().unwrap().get(&id).cloned()
```
to:
```rust
store.lock().unwrap().get::<Plan>(&id)?
```

All `*.list` handlers change from:
```rust
stores.plans.read().unwrap().values().cloned().collect()
```
to:
```rust
store.lock().unwrap().list::<Plan>(&filters)?
```

### DaemonContext Changes

```rust
pub struct DaemonContext {
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub config: Config,
    pub store: Arc<Mutex<taskstore::Store>>,    // replaces Arc<Stores>
    pub worktree_manager: WorktreeManager,
    pub validator: Option<DocValidator>,          // None if validator.enabled=false
}
```

Since TaskStore is synchronous and daemon handlers run in an async context, we need to handle the sync/async boundary carefully.

**Key observation:** `dispatch()` in `handlers.rs` is a synchronous function. The async boundary is at the socket read/write level in `daemon/mod.rs`. This means:
- Replace `&Arc<Stores>` with `&Arc<Mutex<Store>>` in `dispatch()` signature
- Use `std::sync::Mutex` (not `tokio::sync::Mutex`) since handlers are sync
- All store operations happen inside `Mutex::lock()` — fast sub-millisecond operations, no contention concern for single-user

**Error mapping:** TaskStore returns `eyre::Result<T>`. Handlers return `DaemonResponse`. Every store call needs mapping:
```rust
let plan: Option<Plan> = store.lock().unwrap()
    .get::<Plan>(&id)
    .map_err(|e| RpcError::internal(&e.to_string()))?;
```

**Timestamp management:** Before calling `store.update()`, handlers must update the record's `updated_at` field:
```rust
let mut plan = store.lock().unwrap().get::<Plan>(&id)?
    .ok_or_else(|| RpcError::not_found("plan", &id))?;
plan.status = new_status;
plan.updated_at = id::now_millis();
store.lock().unwrap().update(plan)?;
```

**Index rebuilding:** On daemon startup, after `Store::open()` (which auto-syncs from JSONL if stale), call `rebuild_indexes::<T>()` for all 9 record types to ensure the SQLite index is current:
```rust
store.rebuild_indexes::<Plan>()?;
store.rebuild_indexes::<Spec>()?;
// ... for all types
```

### TUI Event Processing

Fix the TODO at `tui/run.rs:99`. When a daemon event arrives:

```rust
match event.event.as_str() {
    "record.created" => {
        let collection = event.data["collection"].as_str().unwrap();
        let id = event.data["id"].as_str().unwrap();
        // Fetch the created record from daemon and add to AppState
        refresh_collection(&mut app.state, &ipc_client, collection).await;
    }
    "transition.completed" => {
        let collection = event.data["collection"].as_str().unwrap();
        refresh_collection(&mut app.state, &ipc_client, collection).await;
    }
    _ => {}  // ignore unknown events
}
```

Strategy: on any event for a collection, re-fetch the full list for that collection. This is simple and correct. The alternative (surgical updates) is more complex and error-prone for MVP2.

### Implementation Plan

**Phase dependencies:** Phase 1 → Phase 2 (handlers need TaskStore). Phase 3 is independent of Phase 2 (can start after Phase 1). Phase 4 depends on Phase 3. Phase 5 is independent (can start after Phase 1). Phase 6 depends on all prior phases.

```
Phase 1 (Foundation) ──→ Phase 2 (Handler Migration) ──→ Phase 6 (CLI & Polish)
    │                                                          ↑
    ├──→ Phase 3 (Doc Validator) ──→ Phase 4 (Validation Gate)─┘
    │
    └──→ Phase 5 (TUI Events) ─────────────────────────────────┘
```

#### Phase 1: TaskStore Integration (Foundation)

1. Add `taskstore` git dependency to `Cargo.toml`
2. Implement `Record` trait for all 8 domain types + `ValidationReport`
3. Replace `Stores` struct with `Arc<Mutex<Store>>` in `DaemonContext`
4. Update `DaemonContext::new()` to call `Store::open()` on startup
5. Call `rebuild_indexes::<T>()` for all 9 record types after open
6. Update `DaemonContext::recover_orphaned_records()` to use TaskStore get/update
7. Run crash recovery after TaskStore load
8. Add `system.init` handler — calls `install_git_hooks()`, confirms collections exist

#### Phase 2: Handler Migration

1. Migrate all `*.create` handlers (8 handlers)
2. Migrate all `*.get` handlers (8 handlers)
3. Migrate all `*.list` handlers (8 handlers)
4. Migrate all `*.transition` handlers (4 handlers)
5. Migrate learning operation handlers (4 handlers: reinforce, contradict, promote, demote)
6. Migrate lock operation handlers (2 handlers: release, expire)
7. Migrate worktree handlers (create, cleanup — the ones that touch stores)
8. Migrate integrator handlers (validate, publish)
9. Update `system.status` to report store stats

#### Phase 3: Doc Validator

1. Add `validator` module with `DocValidator` struct
2. Add `ValidatorConfig` to `Config`
3. Implement LLM API client (HTTP via `ureq`) for Anthropic Messages API
4. Define validation prompt templates for Plan, Spec, Phase
5. Implement structured output parsing → `ValidationReport`
6. Add `validator.validate` handler
7. Add `validator.report` and `validator.reports` handlers
8. Store `ValidationReport` in TaskStore

#### Phase 4: Validation Gate

1. Add validation gate to `plan.transition` (Draft → Active)
2. Add validation gate to `spec.transition` (Draft → Active)
3. Add validation gate to `phase.transition` (Draft → Active)
4. Add `-32003` error code for validation_required
5. Add `--skip-validation` escape hatch for Coordinator (emergency override)

#### Phase 5: TUI Event Processing

1. Implement `refresh_collection()` — sends `*.list` IPC request and updates AppState
2. Wire event handler in `tui/run.rs` to call `refresh_collection` on daemon events
3. Add validation report display to TUI (show latest report for selected record)

#### Phase 6: CLI & Polish

1. Add `loopr init` CLI command (calls `system.init`)
2. Add `loopr validate <collection> <id>` CLI command
3. Add `loopr report <id>` CLI command
4. Update `loopr status` to show TaskStore stats
5. Comprehensive integration tests

### Doc Validator Prompt Design

The validator uses **per-type prompt templates**. Each type has its own evaluation criteria. The template is filled with the document's content:

```
You are a technical document validator for a software development orchestrator.
You are reviewing a {collection_type} document. Your job is to assess whether
this document is complete and clear enough to move from Draft to Active status.

## Document Under Review

Title: {title}
Description:
{description}

{additional_fields_for_type}

## Evaluation Criteria

{criteria_for_type}

## Output Format

Respond with ONLY valid JSON matching this schema:
{json_schema}
```

**Per-type criteria:**

| Type | Criteria |
|------|----------|
| **Plan** | Clear objective stated. Measurable acceptance criteria defined. Scope is bounded (not open-ended). |
| **Spec** | References a valid Plan. Technical approach described. Key decisions documented. Testability addressed. |
| **Phase** | References a valid Spec. Ordered correctly within the Spec. Deliverables are concrete. Dependencies identified. |

**Per-type additional fields:**

| Type | Additional Fields |
|------|-------------------|
| **Plan** | `acceptance_criteria` |
| **Spec** | `plan_id` (resolved to Plan title for context) |
| **Phase** | `spec_id` (resolved to Spec title), `order` |

The `json_schema` contains the `ValidationReport` schema (minus `id`, `created_at`, `model_used` which the daemon fills in after receiving the LLM response).

**LLM client:** `dispatch()` is a sync closure called inside `tokio::spawn` (see `daemon/mod.rs:85-93`). This creates a critical constraint:

- **`reqwest::blocking` is NOT an option.** It panics with "Cannot start a runtime from within a runtime" when called inside a Tokio task, because `reqwest::blocking` internally spawns its own Tokio runtime.
- **`tokio::task::spawn_blocking`** can't be used from a sync context without an async bridge.

**Decision:** Use `ureq` — a minimal, purely synchronous HTTP client with no async runtime. It's purpose-built for this scenario: sync HTTP calls from within an async context.

```toml
ureq = { version = "3", features = ["json"] }
```

The `DocValidator` makes a blocking `ureq::post()` call. Since `dispatch()` runs on a Tokio worker thread and the daemon is single-user, blocking for 1-10 seconds during validation is acceptable. No other requests are queued.

## Alternatives Considered

### Alternative 1: Async TaskStore Wrapper

- **Description:** Write an async wrapper around TaskStore using `tokio::task::spawn_blocking` for every operation, making all handler calls async.
- **Pros:** Consistent async surface across the daemon. Won't block the Tokio runtime on disk I/O.
- **Cons:** Adds complexity. Every handler becomes async. Error types need conversion. TaskStore operations are fast (SQLite + append-only JSONL) — blocking time is sub-millisecond.
- **Why not chosen:** The dispatch function is already synchronous. TaskStore operations are fast enough that blocking is not a concern for a single-user local daemon. Keep it simple.

### Alternative 2: Keep HashMaps + Periodic Snapshot

- **Description:** Keep in-memory HashMaps as primary store. Periodically serialize all state to a single JSON file. Load on startup.
- **Pros:** No new dependency. Simple implementation. No Record trait to implement.
- **Cons:** No incremental writes (full snapshot on every change or risk data loss). No git merge driver. No indexed queries. No audit trail. Reinvents what TaskStore already does.
- **Why not chosen:** TaskStore exists specifically for this use case. Using it validates the dependency and provides audit trail, merge driver, and indexed queries for free.

### Alternative 3: Embedded LLM Validator (Local Model)

- **Description:** Run validation using a local LLM (e.g., via llama.cpp or ollama) instead of an API call.
- **Pros:** No API key required. No network dependency. No per-call cost.
- **Cons:** Requires local GPU or accepts slow CPU inference. Model quality for structured validation is lower than Claude/GPT. Adds significant binary size or runtime dependency.
- **Why not chosen:** MVP2 is about proving that LLM intelligence can be safely inserted into the spine. API quality matters more than local convenience at this stage. The `provider` config field allows adding local providers later.

### Alternative 4: Validation as Separate Process

- **Description:** Run the Doc Validator as a separate process (sidecar) that the daemon calls via IPC or CLI.
- **Pros:** Isolation. Validator crash doesn't take down daemon. Can be replaced independently.
- **Cons:** Adds deployment complexity. IPC overhead. State sharing between daemon and validator is awkward. For a synchronous gate, the simplicity of an in-process call wins.
- **Why not chosen:** Over-engineering for MVP2. The validator is a pure function: document in, report out. No reason to isolate it into a separate process.

### Alternative 5: TUI Surgical Event Updates

- **Description:** Instead of re-fetching the full collection on each event, parse the event data and surgically update/insert/delete the specific record in AppState.
- **Pros:** More efficient (no round-trip for full list). Lower latency.
- **Cons:** Error-prone (must handle every event type correctly). Dual-write problem (daemon state vs TUI state can diverge). Complex to maintain.
- **Why not chosen:** For MVP2 with a single user and small record counts, re-fetching the full collection is simple and correct. Optimize in MVP3+ when parallelism makes it necessary.

## Technical Considerations

### Dependencies

**New runtime dependencies:**
- `taskstore` — Git dependency from `scottidler/taskstore`. Brings `rusqlite`, `fs2`, `eyre`.
- `ureq` — Minimal sync HTTP client for LLM API calls. No async runtime, no OpenSSL (uses `rustls`). Required because `reqwest::blocking` panics inside a Tokio runtime.

**Dependency compatibility:**
- TaskStore uses `eyre::Result` — matches loopr's existing error handling
- TaskStore uses `serde` — already in use
- `ureq` is sync-only — no conflict with Tokio

### Performance

- TaskStore operations are sub-millisecond (SQLite + append-only file). No performance concern for single-user.
- LLM API call is the only slow operation (1-10 seconds). It's explicitly synchronous and blocking. The user initiates it and waits. No background processing.
- Re-fetching full collections on TUI events adds one IPC round-trip per event. For MVP2 record counts (tens, not thousands), this is negligible.

### Security

- LLM API key is read from an environment variable, never stored in config files or TaskStore.
- Validation prompts contain document content (titles, descriptions). These are developer-written and local-only. No PII concern.
- TaskStore JSONL files are committed to git — same security boundary as the code itself.
- No network exposure beyond the LLM API call (still Unix socket for IPC).

### Testing Strategy

**Unit tests:**
- `Record` trait implementations: roundtrip serialization, indexed fields correctness
- `ValidationReport` parsing: valid JSON, edge cases (empty issues, missing fields)
- Validation gate logic: pass/fail/warn/missing report scenarios
- `DocValidator` prompt construction: correct template filling

**Integration tests:**
- Full lifecycle with persistence: create → restart daemon → verify records survive
- Validation flow: create Plan → validate → transition (pass and fail cases)
- Crash recovery with TaskStore: create records, kill daemon, restart, verify recovery
- TUI event processing: create record via CLI, verify TUI state updates

**Mock LLM for tests:**
- The `DocValidator` accepts a trait-based HTTP client so tests can inject a mock that returns canned `ValidationReport` JSON without hitting a real API.

### Rollout Plan

1. Pillar 1 (TaskStore) ships first. It's independent and immediately useful.
2. Pillar 2 (Doc Validator) ships second. It's opt-in (`validator.enabled = false` by default).
3. TUI event processing ships with Pillar 1 (it's a small fix).
4. No migration needed — MVP1 has no persistent state to migrate. Fresh start.

## Success Criteria

| # | Criterion | How to Verify |
|---|-----------|---------------|
| 1 | Create records, restart daemon, records still exist | `loopr plan create` → kill daemon → restart → `loopr plan list` shows the plan |
| 2 | `.taskstore/` directory contains JSONL files committed to git | After creating records of each type, `ls .taskstore/*.jsonl` shows collection files; `git status` shows them as trackable |
| 3 | `system.init` creates TaskStore and installs git merge driver | `loopr init` → `.gitattributes` contains merge driver rule |
| 4 | Crash recovery works with persistent state | Create InProgress Work → kill daemon → restart → Work is Blocked |
| 5 | `validator.validate` returns structured ValidationReport | `loopr validate plans <id>` → prints report with verdict, issues |
| 6 | Draft → Active blocked without passing validation (when validator enabled) | `loopr plan transition <id> active coordinator` → error -32003 |
| 7 | Draft → Active succeeds after passing validation | Validate → transition → success |
| 8 | `--skip-validation` override works | `loopr plan transition <id> active coordinator --skip-validation` → success |
| 9 | TUI updates reactively on daemon events | Create record via CLI while TUI is open → TUI shows new record without manual refresh |
| 10 | Validator disabled by default, doesn't break existing flow | Default config → transitions work exactly as MVP1 (no validation gate) |

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| TaskStore API doesn't support a needed query pattern | Low | Medium | TaskStore is our own crate; we can extend it. Filter system covers most needs. |
| LLM API latency frustrates user during validation | Medium | Low | Validation is explicit (user initiates). Show spinner in TUI. Cache reports — don't re-validate unchanged docs. |
| LLM produces unparseable JSON despite structured prompt | Medium | Medium | Retry once with error feedback. Fall back to `Verdict::Fail` with raw output in summary. |
| `Mutex<Store>` contention under load | Low | Low | Single-user, serial execution. No contention scenario in MVP2. |
| JSONL files grow large over time | Low | Low | TaskStore handles dedup on sync. Compaction is a future concern (not MVP2). |
| Anthropic API changes break validator | Low | Medium | Pin API version. Abstract behind provider trait for swappability. |
| Validator enabled but API key env var missing | Medium | Medium | Check at daemon startup when `validator.enabled = true`. Fail fast with clear error, not silently at validation time. |
| Stale validation report (doc edited after validation) | Medium | Low | MVP2: reports are not content-aware. The gate only checks if a passing report exists. MVP3+: add content hash to detect staleness. |
| `.taskstore/` appears in worktrees | Low | Low | Daemon opens TaskStore at `config.project.repo_path` (main repo), not at worktree paths. Worktree copies of JSONL files are inert — only the daemon writes to the main repo's store. |

## Open Questions

- [ ] Should `ValidationReport` be a first-class domain type with its own TUI view, or just stored as metadata on the parent record?
- [ ] Should validation be re-run automatically when a Plan/Spec/Phase is edited (content changes), or only on explicit `validator.validate`?
- [ ] Should the `--skip-validation` escape hatch require a reason string for audit trail?
- [ ] What minimum version of TaskStore is required? Are there any missing features we need to add to TaskStore first?
- [ ] Should `.taskstore/` be excluded from worktree copies (via `.gitattributes` or worktree sparse checkout)?
- [ ] Should validation reports include a content hash so the gate can detect when the doc has changed since validation?

## References

- `docs/design/2026-02-25-orchestration-spine.md` — MVP1 design doc (source of truth for spine architecture)
- `docs/mvps.md` — MVP phase comparison table
- `scottidler/taskstore` — TaskStore crate (Record trait, Store, Filter, JSONL + SQLite)
- Anthropic Messages API — `/v1/messages` endpoint for Doc Validator
