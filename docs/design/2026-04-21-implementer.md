# Design Document: Stage 7 Implementer Agent

**Author:** Claude (with Scott)
**Date:** 2026-04-21
**Status:** Draft
**Crates touched:** `domain`, `store`, `context`, `agents`, `llm` (cross-cutting; lives in top-level `docs/design/` per project rules).

## Summary

The Implementer is a Ralph Wiggum loop that takes a ready `Work` item, runs inside a git worktree the coordinator already created, calls the LLM iteratively with a free-form JSON-action prompt, and returns a `Bundle` record when the work is complete. The loop is a direct port of v3/v4's battle-tested implementer, adapted to v5's typed signatures and dependency-injection pattern. No new loop mechanics are invented.

This doc covers two direct prerequisites: the `Bundle` domain type (port from v3/v4 with typed IDs) and the `ContextBuilder` entry point in the `context` crate. It records one required extension to the `llm` crate: a `complete_free` method for the non-tool-forced completion path.

## Problem Statement

### Background

Stage 6 ends with a decomposed `Work` DAG in TaskStore, each item in `Ready` state. Stage 7 makes the first Work item execute: an agent runs in a worktree, calls tools to read and edit code, and proposes a `Bundle` when it believes the acceptance criteria are met.

### Problem

v5 has no Implementer agent. Without it, a decomposed Work item sits forever in `Ready` with no mechanism to transition it, produce code changes, or emit a `Bundle` for the Reviewer.

### Goals

- `run_implementer(work, worktree, deps) -> Result<Bundle>` runs a bounded LLM loop and returns a persisted Bundle.
- `Bundle` + `BundleStatus` domain type with typed IDs and `#[derive(Fsm, Record)]`.
- `BundlesStore<'_>` in `store`: `create`, `get`, `list`, `list_by_work_id`.
- `context::build_for_implementer(...)` assembles system prompt + user message.
- `LlmClient::complete_free(system, messages) -> Result<String>` added to `llm`.
- `AgentAction` enum (Implementer subset): `RunTool`, `CommitChanges`, `ProposeBundle`, `Done`, `NeedHelp`.
- Lifeguard: repeated-action detection + parse-failure tracking.
- Self-correction sub-loop: parse failures append error to message history and re-call LLM (multi-turn). Tool correctable errors do the same within the action loop.
- Force-propose on iteration cap: `git add -u`, commit, return Bundle with `force_proposed: true`.

### Non-Goals

- Reviewer and Integrator agents (Stage 8).
- Parallel Implementer loops (Stage 9+).
- Director/escalation agent. `NeedHelp` returns `Err(EscalationNeeded)`.
- `AdvisorAssistedRetry` strategy.
- Streaming LLM output to IPC clients.
- Rebase-on-merge. First gate is serial.
- Handlebars template loading (deferred; Phase 2 uses inline string rendering).

## Proposed Solution

### Loop contract

The coordinator creates a `Worktree` handle, then calls `run_implementer(work, &worktree, &deps)`. The function loops up to `max_iterations` times. Each iteration:

1. Fetch state summary (rejected bundle reason if retrying).
2. Call `deps.context.build_for_implementer(...)` for `system_prompt` + `user_message`.
3. **Self-correction sub-loop** (multi-turn message history within the iteration): call `deps.llm.complete_free(system, &messages)`. Parse actions. On parse failure, append `(assistant: raw, user: error)` to the local `messages` vec and re-call. Cap at `max_requeries` requeries per iteration.
4. For each parsed action: lifeguard check; emit event; dispatch. On correctable tool error, append `(assistant: [action_json], user: error)` to the same `messages` vec and re-call the LLM once for a corrected action, then execute the corrected action. On non-correctable error, break the action loop and continue to the next iteration.
5. On `ProposeBundle`: `git add -A` + commit if anything is staged; capture HEAD SHA; capture `loc_changed` against the worktree base; construct Bundle; persist; return `Ok(bundle)`.
6. On `Done`: noop Bundle with `noop_reason`; persist; return `Ok(bundle)`.
7. On `NeedHelp`: `git add -u && git commit -m "partial: agent needed help"` (skip if nothing staged); return `Err(EscalationNeeded)`.
8. Accumulate iteration summary into `history`.

On exhausting `max_iterations`: run the force-propose path (see below).

### Invariants

- **Parse-failure counter resets only on successful parse.** `Lifeguard::reset_parse_failures()` is called on the `Ok(actions)` branch inside the sub-loop, never unconditionally after the sub-loop exits. Calling it unconditionally would make the counter permanently stuck at 0 or 1 and the escalation path unreachable.
- **Message history is per-iteration.** Parse-retry and tool-correction both append to the same `messages: Vec<ChatMessage>` that started with the iteration's user message. The vec is dropped at iteration boundary; cross-iteration context is carried by `history: Vec<IterationSummary>`.
- **Lifeguard and iteration cap are independent shutdown paths.** Parse-failure escalation fires when N consecutive iterations exhaust their requery budget on unparseable output. Iteration cap fires when the LLM emits parseable actions that never reach `ProposeBundle`. If garbage starts late (e.g. iteration 18 of 20), `max_iterations` fires before `max_parse_failures` accumulates, preserving prior work.
- **Lifeguard hash is structural, not string-based.** Actions are canonicalized to a sorted-key JSON form before hashing, independent of whether any workspace crate enables the `serde_json/preserve_order` feature.
- **git-add scope is uniform.** `CommitChanges` and `ProposeBundle` both use `git add -A` so new files the agent created are staged. Force-propose uses `git add -u` (conservative: only commits verified tracked modifications).
- **Force-propose file guard escalates.** If force-propose finds more than `max_force_propose_files` (default 100) modified tracked files, or any staged file over `max_force_propose_file_size_bytes` (default 10 MB), the function returns `Err(EscalationNeeded("force-propose guard tripped: {reason}"))`. No Bundle is persisted. The coordinator treats this the same as `NeedHelp`: a human inspects the worktree.
- **`loc_changed` is diffed against the worktree base ref.** `git diff --numstat <base_sha>..HEAD` where `<base_sha>` is captured from `Worktree::base_sha()`. `git diff --numstat HEAD` alone is wrong after the commit lands because HEAD matches the working tree. Binary files (`-\t-\t<file>`) contribute 0 to the total.
- **`complete_free` reads the first text block.** `AnthropicClient::complete_free` searches the response content array for the first block with `"type": "text"`. Thinking blocks (`"type": "thinking"`) are debug-logged and discarded.
- **`list_by_work_id` is indexed.** `Bundle.work_id` carries `#[record(indexed)]`. The lookup uses `Filter { field: "work_id", op: FilterOp::Eq, value: IndexValue::String(...) }` backed by a SQLite index.

### Data Model

#### `Bundle` (`crates/domain/src/bundle.rs`)

```rust
id_type!(BundleId, "bd");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Fsm, strum::Display)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
#[fsm(
    role = crate::Role,
    terminal = [Merged, Rejected, IntegrationFailed, Superseded],
    transitions(
        Proposed          => Triaged           by (Coordinator),
        Proposed          => Rejected          by (Coordinator),
        Proposed          => Superseded        by (Coordinator),
        Triaged           => Reviewed          by (Coordinator, Reviewer),
        Triaged           => Accepted          by (Coordinator),
        Triaged           => Rejected          by (Coordinator, Reviewer),
        Triaged           => Superseded        by (Coordinator),
        Reviewed          => Accepted          by (Coordinator),
        Reviewed          => Rejected          by (Coordinator, Reviewer),
        Reviewed          => Superseded        by (Coordinator),
        Accepted          => Integrating       by (Integrator),
        Accepted          => Superseded        by (Coordinator),
        Integrating       => Merged            by (Integrator),
        Integrating       => IntegrationFailed by (Integrator),
        Integrating       => Superseded        by (Coordinator),
    ),
)]
pub enum BundleStatus {
    Proposed, Triaged, Reviewed, Accepted, Integrating,
    Merged, Rejected, IntegrationFailed, Superseded,
}

#[derive(Debug, Clone, Serialize, Deserialize, Record)]
pub struct Bundle {
    pub id: BundleId,
    #[record(indexed)]
    pub work_id: WorkId,
    pub base_tick_id: Option<String>,
    pub branch_name: String,
    pub paths: Vec<String>,
    pub claims: Vec<String>,
    pub verification: String,
    #[serde(default)]
    pub loc_changed: Option<u32>,
    #[serde(default)]
    pub noop_reason: Option<String>,
    #[serde(default)]
    pub head_commit: Option<String>,
    #[serde(default)]
    pub force_proposed: bool,
    #[record(indexed)]
    pub status: BundleStatus,
    pub created_at: i64,
    pub updated_at: i64,
}
```

Rationale for FSM shape:

- `Reviewer` cannot act on `Proposed` — the Coordinator always triages first. The `Proposed => Rejected` transition is coordinator-only.
- `IntegrationFailed` is a distinct terminal from `Rejected`. A merit-based rejection (reviewer says "this is wrong") is semantically different from an integration failure (merge conflict, post-merge test break) and downstream consumers can branch on it without parsing a verification string.
- `deny_unknown_fields` is intentionally NOT set on `Bundle`. TaskStore's record envelope may add fields; strict deny becomes a migration hazard.

#### `AgentAction` (`crates/agents/src/action.rs`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentAction {
    RunTool { tool: String, input: serde_json::Value },
    CommitChanges { message: String },
    ProposeBundle { claims: Vec<String> },
    Done { message: String },
    NeedHelp { reason: String },
}
```

Note: no `description` field on `ProposeBundle`. The v3 `description` field was removed in v0.1.96 because short optional strings drove retry drift (coordinator regenerated from one-line context). Content authority lives in `docs/loopr/<id>.md`; Bundles reference Work, they do not carry spec content.

#### `ChatMessage` (`crates/llm/src/message.rs`)

```rust
pub struct ChatMessage {
    pub role: String,   // "user" | "assistant"
    pub content: String,
}

impl ChatMessage {
    pub fn user(s: impl Into<String>) -> Self { ... }
    pub fn assistant(s: impl Into<String>) -> Self { ... }
}
```

Lives in `llm` because `LlmClient::complete_free` consumes it in its public trait signature. `llm` depends only on `domain` and `telemetry`; `agents` depends on `llm`. Putting `ChatMessage` in `agents` would require `llm → agents`, which is a cycle.

#### `ContextBuilder` (`crates/context/src/`)

```rust
pub struct AssembledContext {
    pub system_prompt: String,
    pub user_message: String,
    pub token_estimate: usize,
}

pub struct StateSummary {
    pub rejected_bundle_reason: Option<String>,
}

pub struct IterationSummary {
    pub iteration: u32,
    pub actions_summary: String,  // capped at 4000 chars
}

pub trait ContextBuilder: Send + Sync {
    fn build_for_implementer(
        &self,
        work: &Work,
        worktree_path: &Path,
        tool_schemas: &[tools::ToolSchema],
        history: &[IterationSummary],
        state: &StateSummary,
        iteration: u32,
    ) -> Result<AssembledContext, ContextError>;
}
```

`tool_schemas: &[tools::ToolSchema]`, not `&[llm::ToolSchema]`. `context` depends on `tools`, not on `llm` — per `crates/context/CLAUDE.md`, this boundary is mandatory. The `ToolSchema` duplication between `tools` and `llm` is a pre-existing smell that this doc does not resolve; `context`'s trait is pinned to the `tools` version.

#### `Lifeguard` (`crates/agents/src/lifeguard.rs`)

```rust
pub struct Lifeguard {
    action_hashes: HashMap<u64, u32>,   // hash → consecutive count
    consecutive_parse_failures: u32,
    max_repeat: u32,                    // default 3
    max_parse_failures: u32,            // default 5
}

pub enum Verdict { Continue, Escalate(String) }

impl Lifeguard {
    pub fn check_action(&mut self, action: &AgentAction) -> Verdict { ... }
    pub fn record_parse_failure(&mut self) -> Verdict { ... }
    pub fn reset_parse_failures(&mut self) { self.consecutive_parse_failures = 0; }
}
```

`check_action` canonicalizes the action structurally before hashing: for any `serde_json::Value` in the action, keys are recursively sorted into a `BTreeMap` before serialization. This is independent of whether the workspace enables `serde_json/preserve_order` (Cargo features are additive — any transitive dep flipping it on would otherwise silently break cross-iteration dedupe).

#### `ImplementerConfig` (`crates/agents/src/config.rs`)

```rust
pub struct ImplementerConfig {
    pub max_iterations: u32,                     // default 20
    pub max_requeries: u32,                      // default 3
    pub max_parse_failures: u32,                 // default 5
    pub max_repeat_action: u32,                  // default 3
    pub max_force_propose_files: u32,            // default 100
    pub max_force_propose_file_size_bytes: u64,  // default 10 MB
}
```

Flat. No `RetryStrategy` trait or `MaxAttemptsRetry` wrapper — there is one implementation, one default, and nothing is swappable at runtime. If a second strategy ever materializes, trait-ify then.

### API

```rust
// crates/agents/src/lib.rs
pub async fn run_implementer<L, T, S, C>(
    work: &Work,
    worktree: &worktree::Worktree,
    deps: &Deps<L, T, S, C>,
) -> Result<Bundle, ImplementerError>
where
    L: LlmClient,
    T: ToolExecutor,
    S: StoreHandle,
    C: ContextBuilder;

pub struct Deps<L, T, S, C> {
    pub llm: L,
    pub tools: T,
    pub store: S,
    pub context: C,
    pub config: ImplementerConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum ImplementerError {
    #[error("escalation needed: {0}")]
    EscalationNeeded(String),
    #[error("llm error: {0}")]
    Llm(#[from] LlmError),
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("context error: {0}")]
    Context(#[from] ContextError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

// Extension to crates/llm/src/client.rs:
pub trait LlmClient {
    fn complete_with_tool<'a>(...) -> impl Future<...> + Send + 'a;
    fn complete_free<'a>(
        &'a self,
        system: &'a str,
        messages: &'a [ChatMessage],  // last message must be role="user"
    ) -> impl Future<Output = Result<String, LlmError>> + Send + 'a;
}
```

## Implementation Plan

### Phase 1: Bundle domain type + BundlesStore
**Model:** sonnet

- `crates/domain/src/bundle.rs`: `id_type!(BundleId, "bd")`, `BundleStatus` with `#[derive(Fsm, strum::Display)]` and the transition table above, `Bundle` struct, `Bundle::new(work_id, base_tick_id, branch_name, claims)`.
- `crates/domain/src/lib.rs`: re-export `Bundle`, `BundleId`, `BundleStatus`.
- `crates/store/src/bundles.rs`: `BundlesStore<'_>` with `create`, `get`, `list`, `list_by_work_id`. `list_by_work_id` uses `taskstore_traits::Filter { field: "work_id", op: FilterOp::Eq, value: IndexValue::String(id.to_string()) }` against the indexed field.
- `crates/store/src/store.rs`: add `pub fn bundles(&self) -> BundlesStore<'_>`.
- Tests: every listed `BundleStatus` transition; terminal variants return `is_terminal() == true`; invalid transitions return `FsmError`; Bundle serde round-trip; `create` + `get`; `list_by_work_id` returns only matching bundles.

### Phase 2: `LlmClient::complete_free` + `ContextBuilder` for Implementer
**Model:** sonnet

- `crates/llm/src/message.rs`: `ChatMessage { role, content }` with `user` / `assistant` constructors.
- `crates/llm/src/client.rs`: add `complete_free<'a>(&'a self, system: &'a str, messages: &'a [ChatMessage])` to `LlmClient`. `AnthropicClient` POSTs the Messages API with messages array, no `tool_choice`, no `tools`, searches for first `{"type": "text"}` content block. Thinking blocks debug-logged and discarded.
- `crates/context/src/lib.rs`: `ContextBuilder` trait, `AssembledContext`, `StateSummary`, `IterationSummary`, `ContextError`.
- `crates/context/src/implementer.rs`: `impl ContextBuilder for InlineContextBuilder` — renders Work title/AC, tool schema names + descriptions (from `tools::ToolSchema`), state summary, iteration history (each capped at 4000 chars), iteration count, footer. Inline string rendering; no handlebars.
- Tests: `complete_free` with single user message (fake client); `build_for_implementer` with minimal Work + empty history produces non-empty strings containing title and AC; history truncation at 4000 chars.

### Phase 3: AgentAction + parse + dispatch
**Model:** opus

Non-mechanical work (balanced-bracket parsing, git-add scoping, numstat edge handling): needs architectural judgment.

- `crates/agents/src/action.rs`: `AgentAction` enum.
- `crates/agents/src/parse.rs`: `parse_actions(response: &str) -> Result<Vec<AgentAction>>`. Strip markdown fences. Try `serde_json::from_str` directly. On failure, fall back to finding the first `[` and its matching `]` via balanced-bracket counting (not `rfind(']')`, which greedily captures trailing brackets in prose). Normalize `"action"` key to `"type"`. Return `Err(ParseError)` on invalid JSON or empty array.
- `crates/agents/src/dispatch.rs`: `dispatch_action(action, worktree, tool_ctx, store)`:
  - `RunTool`: route through tool registry, return `ActionResult::ToolOutput(stdout)`.
  - `CommitChanges`: `git add -A`; if `git status --porcelain` shows nothing staged, return `NothingToCommit`; else `git commit --message=<msg> --no-gpg-sign`; return `ActionResult::Committed(sha)`.
  - `ProposeBundle`: `git add -A`; commit if staged; `git diff --numstat <worktree.base_sha()>..HEAD` for `loc_changed` (skip rows with `-` in cols 1 or 2); `git rev-parse HEAD` for `head_commit`; construct + persist Bundle; return `ActionResult::BundleCreated(bundle)`.
  - `Done`: noop Bundle with `noop_reason = Some(message)`, persist, return `ActionResult::Done(bundle)`.
  - `NeedHelp`: return `ActionResult::NeedHelp(reason)`.
- `crates/agents/src/lifeguard.rs`: `Lifeguard`, `Verdict`. Action canonicalization (recursive key sort) before FNV hash.
- Tests: `parse_actions` round-trips; markdown fences stripped; balanced-bracket fallback recovers from prose-wrapped JSON; CommitChanges on real tempdir git repo; CommitChanges with nothing staged returns `NothingToCommit`; ProposeBundle captures correct HEAD SHA and non-zero `loc_changed` when files are modified; numstat binary-file rows treated as 0; Lifeguard hash stable across key reorderings (explicit test emits the same action with differently-ordered input keys and asserts hash equality).

### Phase 4: Core `run_implementer` loop
**Model:** opus

- `crates/agents/src/implementer.rs`: full `run_implementer` with state fetch, context assembly, multi-turn self-correction sub-loop, action loop with correctable-error re-prompt, force-propose with `git add -u` + file-count/size guard (over the limit → `Err(EscalationNeeded)`, no Bundle persisted).
- `crates/agents/src/lib.rs`: wire modules, export `run_implementer`, `ImplementerError`, `Deps`, `ImplementerConfig`.
- Integration tests: (a) fake LLM returning `ProposeBundle` on first call → `Ok(bundle)`; (b) fake always returning `[RunTool]` (parseable, never proposes) for 20 iterations → force-propose fires, `Ok(bundle)` with `force_proposed: true`; (c) fake always returning garbage JSON for `max_parse_failures` consecutive iterations → Lifeguard escalates, `Err(EscalationNeeded)`; (d) fake returning `NeedHelp` → `Err(EscalationNeeded)` with partial commit visible in git log; (e) fake that triggers force-propose-guard (101 modified files) → `Err(EscalationNeeded)`, no Bundle in store.

### Phase 5: Seam tests + Architect audit
**Model:** opus

- Seam: real git repo fixture, fake LLM → RunTool(bash, "echo hi") → ProposeBundle; real dispatch; real Bundle in store; `bundle.work_id == work.id`; `bundle.loc_changed` reflects the actual diff against the worktree base.
- Config: `ImplementerConfig` from `.loopr/config.yml` `agents.implementer` key.
- Architect audit (post-implementation): validate loop behavior matches this spec, force-propose guard triggers escalation, Lifeguard hash canonicalization holds under `preserve_order`.

## Alternatives Considered

### Alternative 1: `complete_with_tool` wrapped with an `actions` tool
Define `actions_tool` schema; force the LLM to invoke it; read `tool_call.input["actions"]`. No new `LlmClient` method, but v3/v4 tested free-form and showed fewer parse failures than tool-constrained. `complete_free` is a one-method addition and keeps the LLM interaction natural.

### Alternative 2: Implementer creates its own worktree
`run_implementer` calls `deps.worktrees.create(work_id, base_sha)` internally. v3/v4 both create the worktree before spawning the agent; `AttemptCleanupPolicy` is a coordinator concern. Same boundary as v3/v4.

### Alternative 3: Large `AgentAction` enum (full v3/v4 set, 20+ variants)
Port all variants including AcquireLock, SpawnAgent, CreateRecord, etc. Dead code at first gate; every unused path must be tested. Five variants cover first-gate needs. Extend on real run failure.

### Alternative 4: `git add -A` for force-propose
Force-propose uses `-u` (tracked-only) intentionally. If the agent stalled out creating garbage files (recursive outputs, binaries), `-A` would permanently write them to the git object store. `-u` on force-propose plus the file-count/size guard is the conservative combination.

### Alternative 5: Force-propose guard emits a zombie Bundle
Persist a `force_proposed: true` Bundle with `head_commit: None` and `paths: []` so the Reviewer can auto-reject it. Rejected: a Bundle with no commit is a contract violation, not a valid state. Guard trip returns `Err(EscalationNeeded)` — same class as `NeedHelp`.

## Technical Considerations

### Dependencies

- `agents`: `tokio` (full), `serde_json`, `tracing`, `thiserror`, path deps on `worktree`/`llm`/`tools`/`store`/`context`/`domain`.
- `context`: `serde`, `tracing`, path deps on `domain`/`store`/`tools`. Does NOT depend on `llm`.
- `llm`: `ChatMessage` added; no new external deps.
- `domain`: `strum` (already used for `PlanStatus`/`WorkStatus`).

### Performance

LLM latency dominates (~10 min worst case for 20 iterations). Acceptable for first gate. `IterationSummary` capped at 4000 chars. Token estimate logged per iteration.

### Security

- `CommitChanges` message passed as `Command::arg` (no shell interpolation).
- `RunTool` routes through `LaneRouter` + `BashDenylist` + sandbox from `tools`.
- `ProposeBundle` records `head_commit` from `git rev-parse HEAD`.
- `complete_free` omits `tool_choice` and `tools` from the API request.
- Force-propose guard prevents runaway git object store growth by escalating instead of committing.

### Testing Strategy

**Unit:** BundleStatus FSM (every transition); Bundle serde; `parse_actions` variants; Lifeguard escalation; Lifeguard hash canonicalization (same action, different key order → same hash); `git diff --numstat` parsing including binary-file rows.

**Integration (real git repo, GPG disabled via `commit.gpgsign = false`):** CommitChanges commit/skip; ProposeBundle captures HEAD and base-diff `loc_changed`; force-propose guard escalates on >100 files; NeedHelp commits partial work.

**Seam:** Full `run_implementer` round-trip with fake LLM → Bundle in store.

### Rollout

Single branch (`v5`), per-phase commits, each passing `otto ci`. The `Execute` subcommand in `loopr` currently returns `StageUnimplemented`; its body is replaced with coordinator wiring after Phase 4.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `complete_free` returns multi-block response with thinking block first | Med | Med | Search for first `type: "text"` block; thinking blocks discarded. |
| LLM repeatedly returns identical correctable tool errors | Med | Med | Lifeguard `check_action` hash detects repeated identical actions; escalates after `max_repeat_action`. |
| `CommitChanges` fails due to pre-commit hook | Med | Low | Agent sees the non-zero exit as a non-correctable dispatch failure; outer iteration continues. No `--no-verify`; agent must work with hooked repos. |
| Transitive dep enables `serde_json/preserve_order`, breaking Lifeguard dedupe | Low | Med | Canonicalize actions (recursive key sort) before hashing; explicit test pins the invariant. |
| `BundleStatus` FSM rejects a listed transition | Low | High | Phase 1 unit test exercises every transition. Compile-time + runtime FSM checks. |
| `git diff --numstat` binary-file rows | Low | Low | Binary files show `-\t-\t<file>`. Skip rows with `-` in cols 1 or 2. |
| Force-propose guard trips frequently in practice | Low | Med | Escalates to human rather than storing junk. If noise is high, revisit the thresholds in config. |

## Open Questions

None blocking.

## References

- `crates/agents/CLAUDE.md` — `Deps<L,T,S,C>` pattern, scope rules
- `crates/context/CLAUDE.md` — ContextBuilder scope, `llm`-independence rule
- `crates/llm/CLAUDE.md` — transport-only boundary
- `crates/llm/src/client.rs` — current `LlmClient` trait
- `crates/domain/src/work.rs` — `#[record(indexed)]` pattern, historical `description`-field removal (v0.1.96)
- `crates/tools/src/schema.rs` — `tools::ToolSchema`
- `crates/worktree/src/handle.rs` — `Worktree` handle API
- `crates/store/src/works.rs` — `list` pattern using `taskstore-async::Filter`
- `docs/design/2026-04-21-worktree-lifecycle.md` — Stage 7 worktree (Implemented, v0.5.23)
- v3 source (port origin, read-only reference): `~/repos/scottidler/loopr/src/agents/implementer.rs`, `~/repos/scottidler/loopr/src/domain/bundle.rs`, `~/repos/scottidler/loopr/src/agents/lifeguard.rs`, `~/repos/scottidler/loopr/src/agents/context.rs`
