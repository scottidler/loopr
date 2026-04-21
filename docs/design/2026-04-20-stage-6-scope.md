# Stage 6 Scope Memo: Decomposer Produces a Work DAG

**Status:** Resolved — Architect consulted (round 1), decisions below are locked.
Next step: write `crates/domain/docs/design/2026-04-20-hierarchy.md`.

**Author:** Claude (with Scott)
**Date:** 2026-04-20
**Architect round:** 1 (`architect.log` 2026-04-20)

This memo gates the writing of Stage 6's design docs. Per `docs/roadmap.md`:

> **Goal:** daemon, on receiving a Plan request, runs the decomposer which
> produces a trivial Work DAG (single Work is fine for now). Work records land
> in `.loopr/taskstore/works.jsonl` with dependencies on the Plan.
>
> **Exit criterion:** `loopr plan "Add --version flag to a Rust CLI"` produces
> at least one Work record persisted to `.loopr/taskstore/works.jsonl`.

The failing run today is the Stage 5 exit state: `loopr plan "x"` persists a
Plan, but nothing decomposes it.

## Lineage: v3 primary, v4 cautionary

v3 (`~/repos/scottidler/loopr`, branch `main`, versions `v0.1.*`) was the
"kinda working" implementation. v4 (`~/repos/scottidler/loopr-v4`) attempted
an architectural re-write (YAML-FSM runtime, agent-harness decomposer,
richer record shapes) and failed — which is why v5 is a clean break. The
Stage-5 work already lifted v3's PlanStatus table via the `#[derive(Fsm)]`
macro (also revived from v3's `loopr-derive`). Stage 6 continues that
posture: lift v3 shapes verbatim; diverge only where the v4 post-mortem
identified a real defect.

Files read for this memo:
- **v3:**
  - `src/domain/work.rs` — Work record, 9-state `WorkStatus` FSM via
    `#[derive(Fsm)]`
  - `src/decomposer.rs` — standalone decomposer function (not an agent)
  - `src/agents/llm_client.rs` — Anthropic client (same SSE pattern as v4)
  - `loopr-derive/src/lib.rs` — `Fsm` + `FlexibleEnum` derive macros
- **v4 (for contrast):**
  - `src/domain/work.rs`, `resources/engine/fsm/work.yml` — YAML runtime
    `WorkStatus` with 10 states (adds `Superseded`) and `BlockedReason` enum
  - `src/daemon/handlers/decomposer.rs` + `src/agents/decomposer.rs` —
    agent-harness split
  - `resources/decompose/roles/{full,brief}.yml`, `resources/decompose/
    work/prompt.pmt` — role configs and prompt templates

## v3 Patterns (the primary lineage)

### Records

- `Plan.parent_id: Option<String>` always `None`. `Work.parent_id: String` is
  the id of whatever produced it (`pl-*` in brief / flat mode, `ph-*` in the
  Full mode that Stage 6 does not yet exercise).
- `Work` fields (v3 verbatim):
  `id, parent_id, title, assignee: Option<String>, status (private, FSM),
  dependencies: Vec<String>, files: Vec<String>, acceptance_criteria,
  attempt_count: u32, session_failure_count: u32, created_at, updated_at`.
  No `description` (removed v0.1.96). No `blocked_reason` — that's a v4
  addition.
- `Plan` carries `tier: Tier::{Full, Brief}` (so brief mode is a Plan attribute,
  not a record-shape choice) and retry counters (`decomposition_attempts`,
  `bubble_up_count`).

### `WorkStatus` FSM (v3, 9 states)

Derive-macro form; each variant carries `#[transitions(...)]` and optional
`#[overrides(...)]` attributes. This is the shape v5's `Fsm` macro already
accepts.

```text
Draft        → Pending(Coordinator), Ready(Coordinator), Abandoned(Coordinator)
Pending      → Ready(Coordinator), Abandoned(Coordinator)
Ready        → InProgress(Coordinator), Blocked(Coordinator),
                Abandoned(Coordinator), Done(Coordinator)
InProgress   → Blocked, InReview(Implementer), Abandoned(Coordinator)
                overrides: Ready(Coordinator), InReview(Coordinator)
Blocked      → Ready(Coordinator), Abandoned(Coordinator)
InReview     → InProgress(Coordinator), Integrated(Integrator),
                Abandoned(Coordinator)
                overrides: Ready(Coordinator)
Integrated   → Done(Coordinator, Integrator), Abandoned(Coordinator)
Done         (terminal)
Abandoned    (terminal)
```

v4 added a 10th state (`Superseded`) for a "reset this Work, create a
replacement" path distinct from `Abandoned`. Neither the v4 post-mortem nor
Stage 6's exit criterion motivates it. **Lift v3's 9 states.**

### Decomposer — a standalone function, not an agent

v3's `src/decomposer.rs` top comment:

> Standalone plan decomposer: receives plan markdown as a string, calls an
> LLM, and builds the full Plan/Spec/Phase/Work hierarchy in memory. This
> is a system call (function), NOT an agent. It has no session, FSM, or
> iteration loop. The Coordinator invokes it before execution begins, and
> can re-invoke it for targeted re-decomposition during execution.

This is the shape Stage 6 wants. v4 moved it to an agent harness that
dispatched through the daemon's IPC bridge back to a handler that called the
LLM — three layers where one sufficed. v5 should match v3's synchronous
function: the daemon's `plan.create` handler calls
`decomposer::decompose(&plan, &llm) -> Result<Vec<Work>>` directly and
persists the returned Works via `ctx.store.works().create_many(works)`.

### LLM contract: Anthropic tool-use + fallback

Both v3 and v4 used Anthropic's tool-use API for structured output:

```json
{
  "tools": [{"name": "submit_decomposition", "input_schema": {...}}],
  "tool_choice": {"type": "tool", "name": "submit_decomposition"}
}
```

The tool schema enforces `{title, content, dependencies, acceptance_criteria}`
per child. `MIN_GENERATION_TOKENS = 8192` to avoid `max_tokens`-truncated
tool-input JSON. Fallback text-parse path exists for when the model ignores
the tool (rare but not never).

### Deps as titles, resolved server-side, cycle-detected

LLM emits `dependencies: [title_str, …]`. Server-side builds a
title-to-sibling-id map (case-insensitive), rewrites into
`Vec<String>` of ids. Topological sort detects cycles; unresolved titles
(LLM hallucinated a sibling) are a hard error. Both v3 and v4 do this.

### Markdown emission — v3 path (later deprecated)

v3 persisted each decomposed child's `content` string to
`docs/loopr/<id>.md` alongside the JSONL record. v4 kept that. v5 has
explicitly moved every loopr-owned path under `.loopr/`, so the v3 location
is wrong for us. Memory `project-docs-loopr-design` tracks that v4's
markdown emission was still being refactored. **Stage 6 skips markdown
emission.** Exit criterion is "Work record persisted," not "human-readable
docs." When we earn it, the files should land at `.loopr/docs/<id>.md` or
similar.

## Decisions

D1 is user-decided. D2–D10 are the architect's to weigh in on.

### D1. `parent_id` typing (user-decided)

Match v3/v4 exactly: `Plan.parent_id: Option<String>` (always `None`),
`Work.parent_id: String` (opaque; `pl-*` in brief mode, `ph-*` later).
Not a typed `PlanId` at this layer because in Full mode the parent is a
Phase, not a Plan. **Resolved.**

### D2. `WorkStatus` states: v3's 9 or v4's 10?

**Proposal:** v3's 9 states verbatim. `Superseded` is a v4 addition that
doesn't pay for itself — the `Abandoned` terminal covers the "stop this
Work" case, and "create a replacement" can be a `new Work with
dependencies: [old.id]` without a new state.

**Risk:** if Stage 7/8 reveals Superseded is actually load-bearing (e.g.
Reviewer rejects a Bundle's Work and we need to distinguish "this Work was
wrong" from "this Work was abandoned by user/director"), we pay a FSM
migration. The `Fsm` derive makes that migration a compile-time error-
surfaced change — acceptable cost.

### D3. v4-only fields on `Work`: ship or defer?

`blocked_reason: Option<BlockedReason>` is the only v4 addition to the v3
record. BlockedReason has three variants: `DependencyWait`,
`ExhaustedRetries`, `SystemFault`.

**Proposal:** defer. v3 ran (kinda) without it. If Stage 7's reactive
coordinator needs to discriminate blocked-reasons, add then. The Record
schema extension is additive; no data migration.

### D4. v3 fields unused at Stage 6: ship or defer?

`attempt_count`, `session_failure_count`, `files`, `assignee`. None are
populated by the decomposer; all are populated by Stage 7+ writers.

**Proposal:** ship verbatim from v3 with `Default` values on birth. Prevents
a Record-schema migration in Stage 7 that clippy would flag for each
existing JSONL row. The v3 source has the fields; v5's port matches.

### D5. Decomposer output: include `content` markdown field?

v3/v4 both emit `content: String` (full markdown of the Work document) in
the LLM response, then persist to `.md` file. Stage 6 does not emit the
`.md`, but the LLM output shape still includes it.

**Proposal:** keep `content` in the LLM tool-schema response (v3 verbatim)
so the prompt is stable across later stages when we do start writing `.md`.
Stage 6 code ignores the `content` field after extracting
`acceptance_criteria` from it (v3 helper `extract_acceptance_criteria`).
**Do not store `content` on the Work record.** The `.md` file lives at
`.loopr/docs/<id>.md` when we earn that path; until then it's discarded.

### D6. LLM backend: port v3's full client or start minimal?

Stage 6 needs one LLM call per decomposition. Stage 7 needs streaming +
tool-use + multi-turn history. v3's `AgentLlmClient` has all three in ~600
lines (v4's is ~1000 with TUI broadcast channel — skip).

**Proposal:** Stage 6 ships a minimal `LlmClient` trait in `crates/llm`
with one method:

```rust
pub trait LlmClient {
    fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<ToolCall, LlmError>> + Send;
}
```

Non-streaming Anthropic backend using `reqwest` (no SSE). The decomposer
calls `complete_with_tool` with the `submit_decomposition` schema and gets
a typed `ToolCall` back. SSE + the second method (`complete_streaming` for
text, `complete_agentic` for multi-turn + tools) lands in Stage 7.

**Risk:** blocking for long LLM calls holds the request handler. Stage 6's
daemon already is async (tokio); `reqwest` buffered call is fine. Ship.

### D7. Tool-use vs text+JSON: ship tool-use?

**Proposal:** tool-use + fallback (v3 verbatim). Tool-use is strictly
better (structured output, schema enforcement). The fallback catches the
rare case where the model ignores the tool and emits `[{...}]` as text.
Cost: ~40 lines. Worth it.

### D8. Dep-resolution shape: titles → IDs with cycle detect?

**Proposal:** v3 verbatim. Titles on the wire, server-side title→id map
(case-insensitive, trim), cycle-detect via topological sort, unresolved
titles are a hard error.

### D9. Workspace file-tree injection: ship at Stage 6?

v3 injects `git ls-files` output into the prompt so the LLM doesn't say
"create from scratch" for pre-existing files. ~15 lines.

**Proposal:** ship. Stage 9's rust-version target starts with
`Cargo.toml`, `src/main.rs`, etc.; without the listing the LLM will tell
the implementer to `cargo new`.

### D10. Atomic persist: does `taskstore-async v0.5.0` support `create_many`?

v3/v4 both used `Store::create_many(Vec<T>)` for atomic batch persist of
decomposed children. If `taskstore-async v0.5.0` lacks this, Stage 6
either (a) persists serially with reconciliation-on-startup for partial
writes, or (b) requests the upstream addition.

**Action:** verify the API before writing `hierarchy.md`. If missing, my
lean is (a) with a note in `hierarchy.md` Open Questions asking for (b)
in the follow-up release.

### D11. Stage 6 design doc count and order

Roadmap lists four docs: `domain/hierarchy.md`, `llm/llm-client.md`,
`agents/context-builder.md` (should be `context/...`), and
`decomposer/plan-then-decompose.md`.

**Proposal:** three docs, not four. Defer `context-builder.md` to Stage 7.
Stage 6's decomposer assembles its own prompt (prompt template + parent
content + workspace file tree); a shared context-builder is earned the
second time a consumer needs it, which is Stage 7's agents.

Order:

1. `crates/domain/docs/design/hierarchy.md` — `Work` record + `WorkStatus`
   FSM (v3's 9 states verbatim). Blocks the other two.
2. `crates/llm/docs/design/llm-client.md` — `LlmClient` trait +
   tool-use-only Anthropic backend.
3. `crates/decomposer/docs/design/plan-then-decompose.md` — function
   signature `decompose(plan: &Plan, llm: &dyn LlmClient) -> Result<Vec<Work>>`,
   prompt assembly, title→id resolution, cycle detection.

## Architect round 1: findings

**Flipped by Architect, accepted:**

- **D2 → 10-state WorkStatus, not 9.** v5's `PlanStatus` already contains
  `Superseded` (lifted from v3's `HierarchyStatus`). v3 had an asymmetry —
  Plan/Spec/Phase can be `Superseded`, Work cannot — which v4 unified by
  adding it to `WorkStatus`. v3's asymmetry is a defect, not a feature:
  a Work whose parent goal was reformulated should be `Superseded` (logical
  replacement), not `Abandoned` (dropped). Match v5's `PlanStatus`
  symmetry and adopt all 10 states.

- **D1 → `Work.parent_id: PlanId`, not `String`.** My "resolved per user"
  tag was wrong: the user was describing v3/v4 shape. v5's `Plan` record
  does not have `parent_id` at all (already shipped that way in Stage 5)
  and opaque `String` throws away `PlanId`'s typed-newtype tooling. For
  Stage 6 the parent is always a Plan, so use `PlanId` directly. When
  Phase lands, either promote to a `ParentId` enum or keep `Work` flat
  under Plan permanently — earn that decision then.

**D10 refinement:** `create_many` exists in `taskstore-async v0.5.0`
(verified at `crates/taskstore-async/src/store.rs:116`). Use it for the
Works batch persist. The cross-file transaction spanning `plans.jsonl`
(Plan status) and `works.jsonl` (new records) still lacks atomicity —
`taskstore` has no cross-file primitive. Reconciliation-on-startup is
required regardless and lives in Stage 7's design doc; `hierarchy.md`
will flag this in Open Questions.

**Architect-added decisions (all accepted):**

- **Zero-children:** bail loudly (v3 path), not auto-Complete (v4 path).
  An LLM that produces zero Works for a user-filed Plan is an anomaly;
  silent success swallows hallucinations.
- **Idempotency:** decomposer must be safe to re-invoke. Stage 7
  crash-recovery clears partial-Works state on a Plan that was mid-
  decomposition at restart, then re-runs the call. Out of scope for
  Stage 6 code; in scope for Stage 6 doc Open Questions.
- **LLM error taxonomy:** typed enum distinguishing `Retryable`
  (rate limits, network drops, transient 5xx) from `Fatal`
  (context-limit exceeded, auth failure, schema validation failure).
  Not a bare `RpcError::Internal(String)` dump.

## Resolved-by-user answers to remaining open questions

After the Architect round, five questions remained for the user; all
resolved:

- **LLM config surface:** goes into `.loopr/config.yml`. `loopr init`
  writes a default config.yml (shipped as a canonical template in
  `resources/config/default.yml` in the loopr-v5 repo). Hardcoded
  defaults in the template: `model: claude-sonnet-4-6`,
  `max-tokens: 8192`, `temperature: 0.3`.
- **API key source precedence:** CLI `--api-key` > env var > config.yml.
  config.yml stores the env var NAME (e.g. `api-key-env: ANTHROPIC_API_KEY`)
  rather than the literal key — keys must never be serialized to disk.
- **HTTP client lifetime:** one `reqwest::Client` per daemon, owned by
  `DaemonContext` alongside `Store`. Ad-hoc reconsideration allowed only
  when a concrete reason emerges.
- **Generics vs `dyn`:** generics for DI (matches `rules/rust.md`). Never
  `&dyn LlmClient` unless advocating for it explicitly.
- **Prompt source:** shipped via `include_dir!()` embedded in the binary;
  `loopr init` writes them to `.loopr/prompts/` on first run for user
  editing. Resolution order (v4 layout, vision.md-sanctioned):
  target `.loopr/prompts/<path>.pmt` > XDG user > baked-in default.
  Decomposer prompts are themed `decompose/…` per v4. Stage 6 uses one:
  `decompose/work/prompt.pmt` (v3 content, ported).

## Ripples into adjacent stages

Two items Stage 6 surfaces that belong to their own stages:

1. **Stage 5's `loopr init` gains write-config + write-prompts.** The
   roadmap's Stage 5 description only mentions `.loopr/`, `.taskstore/`
   (now `.loopr/taskstore/`), and git hooks. Stage 6 needs the init
   command to also write `.loopr/config.yml` (from the repo's default
   template) and `.loopr/prompts/**/*.pmt` (from the `include_dir!()`
   snapshot). Track as a Stage 5 extension, not as Stage 6 scope.
   Stage 6 code falls back to baked-in defaults when the target has not
   been init'd so that testing and ad-hoc commands do not require init.

2. **Stage 7's `loopr init` gains hook install + worktree-registry
   scaffolding.** Out of scope for Stage 6; noted for completeness.

## Final decision matrix

| # | Decision | Branch |
|---|---|---|
| D1 | `Work.parent_id` typing | `PlanId` (not `String`, not `Option`) |
| D2 | `WorkStatus` states | 10 verbatim (v4 shape, matches v5 `PlanStatus`) |
| D3 | `blocked_reason` on `Work` | defer |
| D4 | Other v3 Work fields | ship all with `Default` |
| D5 | Markdown `content` emission | discard after AC extraction; no `.md` file |
| D6 | `LlmClient` trait shape | tool-use only, non-streaming, buffered |
| D7 | Structured output contract | tool-use + text fallback |
| D8 | Deps on wire | titles → server-side id map → cycle detect |
| D9 | Workspace file tree injection | ship (`git ls-files`) |
| D10 | Works batch persist | `create_many` (verified v0.5.0) |
| D11 | Stage 6 design doc count | 3: `hierarchy` → `llm-client` → `plan-then-decompose` |
| A+1 | Zero-children | bail |
| A+2 | Idempotency | reconcile-on-restart (Stage 7 scope) |
| A+3 | LLM error taxonomy | typed `Retryable` / `Fatal` enum |
| U+1 | LLM config surface | `.loopr/config.yml`, init writes template |
| U+2 | API key precedence | CLI > env > config-yaml-env-name |
| U+3 | HTTP client lifetime | one per daemon, owned by `DaemonContext` |
| U+4 | Trait DI shape | generics, never `&dyn` without cause |
| U+5 | Prompt source | `include_dir!()`, init writes to `.loopr/prompts/`, 3-layer resolution |

## Not a Design Doc

This memo is a scope gate. The three Stage-6 design docs each reference
this matrix by row number rather than re-litigating decisions.
