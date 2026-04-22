# Design Document: Stage 6 Decomposer (plan-then-decompose)

**Author:** Claude (with Scott)
**Date:** 2026-04-20
**Status:** Implemented
**Review Passes Completed:** 5/5 + Architect round 2 (pre-design Q&A on fallback path + workspace-tree injection)
**Shipped in:** v0.5.20

## Deviations from spec (recorded at ship time)

- **Phase 5 API-key handling:** the spec said the daemon should "fail fast with `FatalReason::ConfigInvalid` if no API key is available" (see the Phase 5 block below). Shipped instead with a `PLACEHOLDER_API_KEY = "unset-placeholder"` fallback in `crates/loopr/src/config.rs`: when the env var named by `llm.api_key_env` is unset, the daemon still boots, any real LLM call fails at call time with 401, and `plan.create` persists the Plan but the decomposer errors and no Works land. This keeps the existing smoke-test suite running on CI without `ANTHROPIC_API_KEY` set, and matches the "Plan remains persisted on decompose failure" contract (scope memo A+2). Real users who configure `ANTHROPIC_API_KEY` or `.loopr/config.yml:llm.api-key-env` get real decomposition. Revisit when Stage 7's reconcile-on-restart lands: that's the natural place to turn "daemon boots without key" into a hard requirement if it proves footgun-y.
- **Phase 6 handler-level happy-path seam test:** the spec called for a daemon/smoke test that asserts `ctx.store.works().list()` returns at least one Work after `plan.create` succeeds. Shipped with the failure-path seam only (`plan_create_with_failing_llm_still_persists_plan_and_leaves_works_empty`). The happy path would require either a wiremock HTTP server or swapping `DaemonContext.llm: Arc<AnthropicClient>` for a trait object (`Arc<dyn LlmClient>`) so tests can inject a `MockLlmClient` — and `Arc<dyn LlmClient>` needs either the `Arc<T>`-blanket-impl or async-trait that the `llm` design doc explicitly defers to Stage 7. Equivalent coverage is split across `src/decompose/tests.rs` (decompose produces Works when LLM succeeds) and `store/tests/works.rs` (`create_many` persists). Fold the happy-path seam in when Stage 7 introduces the `Arc<dyn LlmClient>` seam for agent orchestration.
**Scope gate:** [`docs/design/2026-04-20-stage-6-scope.md`](../../../../docs/design/2026-04-20-stage-6-scope.md) — decisions D6, D7 (refined, see below), D8, D9, D10, A+1, A+2, A+3, U+1-U+5 are locked there and referenced by row rather than re-litigated.

**Cross-crate scope note:** This doc's primary subject is the `decomposer` crate (~6 new source files), but Phase 1 adds a `WorksStore` to `store` (~1 new file + 2-line edit) and Phase 5 wires `plan.create` in `loopr` (~one-function edit + `DaemonContext` field). Per `crates/decomposer/docs/CLAUDE.md`'s rule "designs touching a second crate go in `../../../docs/`," a strict reading would move this doc to `docs/design/`. It stays here because: (1) the roadmap explicitly names this location (`docs/roadmap.md:102`); (2) the `store` and `loopr` edits are additive wiring (no shape change to existing types), not cross-cutting architecture; (3) the decomposer is unambiguously the architectural subject. If Phase 1's `WorksStore` review surfaces a non-trivial store-layer decision, split it into its own `crates/store/docs/design/works-store.md` and reference from here.

## Summary

Introduce a `decompose<L: LlmClient>(plan, target, llm) -> Result<Vec<Work>, DecomposerError>` function in `crates/decomposer/`. It builds a prompt (template + parent Plan goal + workspace file tree from `git ls-files`), calls the just-shipped `LlmClient::complete_with_tool` with a `submit_decomposition` tool schema, resolves title-based sibling dependencies to `WorkId`s server-side, detects cycles, and returns a `Vec<Work>` ready for batch-persist. A single retry-with-error-in-prompt covers any LLM failure; zero children bails loudly. The daemon's `plan.create` handler calls `decompose()` between persisting the `Plan` and returning, and persists the resulting `Work`s via `Store::works().create_many(...)` — which lands in this doc because `store` does not yet expose `works()`.

This is the third and final Stage 6 design doc; [`hierarchy.md`](./2026-04-20-hierarchy.md) shipped the `Work` record, [`llm-client.md`](./2026-04-20-llm-client.md) shipped the `LlmClient` trait + Anthropic backend, and this doc wires them into `loopr plan`. Shipping it satisfies Stage 6's exit criterion: `loopr plan "Add --version flag to a Rust CLI"` produces at least one `Work` persisted to `.loopr/taskstore/works.jsonl`.

## Problem Statement

### Background

Stage 5 ships a daemon whose `plan.create` handler persists a `Plan` and returns. Stage 6's goal is that the same call produces at least one `Work` record. Two of the three Stage-6 design docs have shipped: `hierarchy.md` defined the `Work` record + 10-state `WorkStatus` FSM, `llm-client.md` defined the `LlmClient` trait and its `AnthropicClient` implementation. Neither is called from any production code; `crates/decomposer/src/lib.rs` is a one-line module comment. This doc fills the hole.

The v3 shape to port (scope memo's locked choice over v4's agent-harness): `src/decomposer.rs` is a standalone async function, ~2000 lines mostly for Spec/Phase/Work hierarchy that Stage 6 does not need. The subset we want is ~200 lines of structure: tool schema builder, title-keyed cycle detection (Kahn's), title→id resolution with case-insensitive fallback, zero-children bail, per-call retry-with-error-in-prompt. v3 wires LLM HTTP inline; v5 separates transport (`llm` crate) from orchestration (`decomposer` crate), so this doc's function is shorter than v3's because `llm` already owns the HTTP and error classification.

### Problem

The daemon has no call site for `LlmClient`. The `Work` record has no producer. The workspace file tree — which the LLM needs to make grounded decomposition decisions — has no collector. And `store::Store` has no `works()` accessor.

### Goals

- A `pub async fn decompose<L: LlmClient>(plan: &Plan, target: &Path, llm: &L) -> Result<Vec<Work>, DecomposerError>` that takes a validated `Plan`, the target-repo path, and any `LlmClient` and returns a batch of `Work`s whose `parent_id` is the `Plan`'s id and whose inter-`Work` `dependencies` are resolved `WorkId`s.
- Tool-use call to `LlmClient::complete_with_tool` with a `submit_decomposition` schema matching v3's shape exactly: `{children: [{title, content, dependencies: [title_str], acceptance_criteria: [str]}]}`.
- One retry on any `LlmError`, re-prompting with `## Previous Attempt Failed\n<error>` appended to the user message. Replaces the scope memo's D7 "text fallback" (refined by Architect round 2; see Alternatives).
- Workspace file tree injected into the prompt via `git ls-files -z --cached --others --exclude-standard` (tracked + untracked-not-ignored), with a `std::fs`-based depth-limited fallback for non-git targets. Entry-count cap at 500 with a truncation suffix naming the overflow size.
- Title-based sibling dependencies resolved server-side (case-insensitive + whitespace-normalized map, unresolved titles are a hard error), followed by Kahn's-algorithm cycle detection.
- Zero-children response bails with `DecomposerError::ZeroChildren` (scope memo A+1).
- `store::WorksStore` with `create(work)` and `create_many(works)` that mirror the existing `PlansStore` anti-corruption pattern, and a `store.works()` accessor.
- Daemon wiring: `plan.create` handler calls `decompose()` between plan persist and return, then `ctx.store.works().create_many(works)`.

### Non-Goals

- **Spec / Phase decomposition.** v3 decomposes Plan → Spec → Phase → Work; scope memo D11 defers Spec/Phase entirely. Stage 6 is flat Plan → Work only.
- **Template / ratification / validation LLM calls.** v3 runs a second LLM call per child (validation with Haiku) and a third call at the end (ratification over the whole hierarchy). Stage 6 runs one call total. Stage 7 can add them when the evaluator lands.
- **Markdown `content` persistence.** Scope memo D5: the LLM returns `content: String` per child to enable AC extraction, but the content is discarded after `AcceptanceCriteria` extraction; no `.md` file is written. Stage 7+ can reintroduce markdown emission under `.loopr/docs/<id>.md`.
- **Idempotent re-decomposition / partial-state recovery.** Scope memo A+2: if the daemon crashes mid-decomposition, Stage 7's reconcile-on-restart clears the half-written state and re-invokes. Stage 6 assumes a clean start every call.
- **Context builder / prompt template engine.** Scope memo D11 defers `context-builder.md` to Stage 7. Stage 6's decomposer assembles its own prompt inline from a hard-coded template string (ported from v3) plus the workspace tree.
- **Concurrent decomposition across multiple Plans.** One Plan at a time; the daemon's `plan.create` handler is serial. Stage 7's reactive coordinator lands parallelism.
- **Config-selectable `DecomposeStrategy`.** `crates/decomposer/CLAUDE.md` mentions `BriefDecompose` / `FullDecompose` strategies. Those are v3 modes (Brief = Plan → Work direct, Full = Plan → Spec → Phase → Work). Stage 6 only has Plan → Work, so there is exactly one strategy — hardcoded. When Spec/Phase land, the strategy selector earns its place.
- **Decomposer config surface beyond what `llm` already exposes.** No new `.loopr/config.yml` keys. The decomposer inherits `LlmConfig` via its `LlmClient` parameter; it has no timeouts, retries-count, or tunable knobs of its own in Stage 6.

## Proposed Solution

### Overview

Six files under `crates/decomposer/src/`:

1. `error.rs` — `DecomposerError` typed enum. Variants: `LlmFailed` (wraps `LlmError`), `ZeroChildren`, `UnresolvedDeps`, `CycleDetected`, `DuplicateTitles`, `WorkspaceScanFailed`, `EmptyAcceptanceCriteria`, `MalformedChildren`. Each maps to a specific caller-distinguishable failure mode.
2. `tool.rs` — builds the `submit_decomposition` `ToolSchema` and deserializes `ToolCall.input` into a typed `DecomposeResponse { children: Vec<DecomposeChild> }`.
3. `tree.rs` — `collect_workspace_tree(target: &Path) -> Result<String, DecomposerError>`; primary `git ls-files` path, `std::fs` fallback, entry cap.
4. `cycles.rs` — `detect_cycles(nodes: &HashMap<String, Vec<String>>) -> Result<(), String>` (Kahn's algorithm; ports v3:126-164 verbatim). Owned `String` keys/values rather than borrowed `&str` because the call site constructs the map from `DecomposeChild.title` / `DecomposeChild.dependencies`, which are owned — trying to borrow would fight the borrow checker for no performance win at n ≤ 5.
5. `prompt.rs` — prompt-string assembly: `assemble_system(tree: &str) -> String` and `assemble_user(goal: &str, prev_error: Option<&str>) -> String`. Template text is a `const &str` literal adapted from v3's `prompts/decompose/work.pmt` (Brief-mode framing + tool-use output substitution).
6. `decompose.rs` — the main `decompose<L: LlmClient>(plan, target, llm)` function orchestrating the others.

And two additional files under `crates/store/src/`:

7. `works.rs` — `WorksStore` with `create`, `create_many`, `get`, `list` methods (mirrors `plans.rs`).
8. Update to `store.rs` adding the `works(&self) -> WorksStore<'_>` accessor.

`crates/decomposer/src/lib.rs` wires the decomposer modules together and re-exports `decompose` + `DecomposerError`.

One edit in `crates/loopr/src/transport/handler.rs::handle_plan_create` to call `decompose()` and persist the results. One edit in `crates/loopr/src/daemon/context.rs` to add an `Arc<AnthropicClient>` field to `DaemonContext` (which already carries `pub target: PathBuf` and `store: Store`).

### Architecture

```
crates/
├── decomposer/
│   ├── Cargo.toml                       (deps already wired: domain, store, llm, context; add tracing, serde_json, tokio)
│   ├── .otto.yml
│   ├── CLAUDE.md                        (existing)
│   ├── docs/
│   │   └── design/2026-04-20-plan-then-decompose.md    (this doc)
│   └── src/
│       ├── lib.rs                       (wire + re-exports)
│       ├── error.rs                     (DecomposerError)
│       ├── tool.rs                      (ToolSchema builder, DecomposeChild/Response serde)
│       ├── tree.rs                      (workspace file tree collection)
│       ├── cycles.rs                    (Kahn's cycle detection)
│       ├── prompt.rs                    (system/user prompt assembly, template const)
│       └── decompose.rs                 (the main function)
├── store/
│   └── src/
│       ├── works.rs                     (new: WorksStore)
│       └── store.rs                     (edit: add works() accessor)
└── loopr/
    └── src/
        ├── daemon/context.rs            (edit: DaemonContext.llm: Arc<AnthropicClient>)
        └── transport/handler.rs         (edit: handle_plan_create calls decompose)
```

Seam boundary: the decomposer is generic over `L: LlmClient`; the daemon passes `&*ctx.llm` (`ctx.llm: Arc<AnthropicClient>` derefs to `AnthropicClient`, which `impl`s `LlmClient`). `&Arc<T>` does not auto-coerce to `&T` at a trait-bound call site, so the `*` is mandatory at the handler. Tests inject a `MockLlmClient` (also heldable as `Arc<MockLlmClient>` in the `DaemonContext` in tests) that returns canned `ToolCall`s. If this ergonomic wart becomes painful, a blanket `impl<T: LlmClient> LlmClient for Arc<T>` in the `llm` crate is a one-function follow-up — deferred until Stage 7 shows it's worth the API surface.

### Data Model

#### `DecomposerError` (`src/error.rs`)

```rust
#[derive(Debug, thiserror::Error)]
pub enum DecomposerError {
    /// The LLM call failed after the single retry. Carries the last
    /// error so callers can inspect whether it was `Retryable` (the
    /// caller may choose to loop again) or `Fatal` (bail to user).
    #[error("LLM call failed: {0}")]
    LlmFailed(#[from] LlmError),

    /// The model returned `children: []`. Scope memo A+1: bail loudly.
    #[error("LLM produced zero child Works for plan {0}")]
    ZeroChildren(PlanId),

    /// One or more children named a sibling title that did not appear
    /// in the same batch. The LLM hallucinated a dependency target.
    /// Included on retry-error-in-prompt so the model can correct.
    #[error("unresolved sibling dependencies: {0}")]
    UnresolvedDeps(String),

    /// Title→id resolution produced a DAG with a cycle among the
    /// named titles. The retry-error-in-prompt includes this list.
    #[error("dependency cycle among: {0}")]
    CycleDetected(String),

    /// `git ls-files` returned non-zero AND the fallback walk also
    /// failed. Unusual: empty target dir is legal and produces a
    /// one-line tree ("(empty workspace)"); this variant is for
    /// permissions / IO-error territory only.
    #[error("workspace scan failed: {0}")]
    WorkspaceScanFailed(String),

    /// A Work's `acceptance_criteria` came back empty and markdown
    /// extraction from its `content` also yielded zero criteria. Per
    /// hierarchy.md, a Work with empty AC would deadlock Stage 7's
    /// `Ready -> InProgress` precondition, so we bail at decompose
    /// time rather than persist the broken record.
    #[error("Work {0:?} has zero acceptance criteria; LLM must produce at least one")]
    EmptyAcceptanceCriteria(String),

    /// The `llm` crate returned a well-formed `ToolCall` (tool-use
    /// block present, `input` is valid JSON), but the `input` did not
    /// deserialize into `DecomposeResponse` — missing `children`
    /// field, wrong per-child shape, non-string `title`, etc. This
    /// is a decomposer-layer structural problem distinct from
    /// `llm::FatalReason::SchemaValidation`, which only catches
    /// "no tool_use block" or "input unparseable as JSON".
    #[error("tool_call input didn't deserialize into decompose schema: {0}")]
    MalformedChildren(String),

    /// Two or more children in the same decomposition normalize to
    /// the same title (after `trim().to_lowercase()`). The
    /// server-side title→id map cannot disambiguate dependency
    /// targets; forcing a retry with an explicit correction in the
    /// prompt. `Vec<String>` carries the colliding normalized titles
    /// so the retry prompt can name them back to the model.
    #[error("LLM produced duplicate child titles: {0:?}")]
    DuplicateTitles(Vec<String>),
}
```

`LlmFailed` is `#[from]` `LlmError` so `?` works at call sites; the retry-with-error-in-prompt path does NOT convert the inner error at retry time (it constructs a fresh prompt string); the conversion only happens at the final `return Err(...)` when the retry also fails.

#### `DecomposeChild` / `DecomposeResponse` (`src/tool.rs`)

```rust
/// Mirrors the tool schema's `children[]` item shape. Field names
/// match the schema the model sees; `#[serde(default)]` on the
/// optional-by-default arrays allows the LLM to omit them for Works
/// with no deps or no pre-extracted AC.
#[derive(Debug, Deserialize)]
pub(crate) struct DecomposeChild {
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub dependencies: Vec<String>, // sibling titles; resolved to WorkIds server-side
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct DecomposeResponse {
    pub children: Vec<DecomposeChild>,
}

/// Build the `submit_decomposition` tool schema. Matches v3's shape
/// verbatim (v3 `decomposer.rs:197-236`): `children` array with
/// per-item `{title, content, dependencies, acceptance_criteria}`,
/// only `title` and `content` are required per-item.
pub(crate) fn submit_decomposition_schema() -> ToolSchema { /* ... */ }
```

#### Prompt template (`src/prompt.rs`, const)

```rust
/// System prompt template, adapted from v3's `prompts/decompose/work.pmt`
/// with the Brief-mode framing (v3 had Brief and Full modes; Stage 6 is
/// Brief-only) and the tool-use output section substituted in place
/// of v3's raw-JSON-array instructions. `{{ TREE }}` is the only
/// substitution; the goal is appended in the user message so the
/// model can distinguish the decomposition instructions (system,
/// static) from the specific Plan (user, dynamic).
const SYSTEM_TEMPLATE: &str = r#"
You are a software architect decomposing a Plan into Work items.

A Work item is the implementer's complete brief — everything needed
to write code without asking a question. Every decision has been made
upstream.

## Output

Use the submit_decomposition tool. For each Work, provide:
- title: Short descriptive title
- content: Full markdown content (template-free; prose is fine)
- dependencies: Titles of sibling Works this one depends on
- acceptance_criteria: Concrete, testable assertions (assert statements)

## Rules

1. Each Work's acceptance_criteria must be concrete, testable assertions
   (assert statements), NOT prose.
2. Dependencies are sibling titles only. The ONLY valid reason for a
   dependency is when a Work literally cannot compile or test without
   another Work's output being present in the repo.
3. Produce 1-5 Work items. Prefer fewer, larger items over many small
   serial items. Two independent items beat five dependent ones.
4. Each Work must have at least one acceptance criterion.

## Parallelism

Work items are discrete chunks that can be built independently and in
parallel. This is their primary design purpose. When decomposing:

- Most work items should have NO dependencies.
- Prefer fan-out: many independent Work items, NOT linear chains.
- STRONGLY AVOID splitting work on the same file across parallel Work
  items. Same-file parallel writes cause merge conflicts.

## Workspace file tree

The target repository currently contains these files:

{{ TREE }}

Use this to ground your decomposition in the actual codebase: name
real files when referring to them, do not propose creating files that
already exist, and do not propose removing files you cannot see.
"#;

pub(crate) fn assemble_system(tree: &str) -> String {
    SYSTEM_TEMPLATE.replace("{{ TREE }}", tree)
}

pub(crate) fn assemble_user(goal: &str, prev_error: Option<&str>) -> String {
    match prev_error {
        None => format!("## Plan\n\n{goal}"),
        Some(err) => format!(
            "## Plan\n\n{goal}\n\n## Previous Attempt Failed\n\n{err}\n\nPlease fix the issues and try again."
        ),
    }
}
```

Stage 7's `context-builder.md` (scope memo D11 deferred) is where `SYSTEM_TEMPLATE` gets extracted into a handlebars-backed prompt engine. Until then: inline.

#### `WorksStore` (`crates/store/src/works.rs`)

Mirrors `PlansStore` with one addition — `create_many`:

```rust
impl<'a> WorksStore<'a> {
    pub async fn create(&self, work: Work) -> Result<WorkId, StoreError> { /* same anti-corruption as plans */ }

    /// Batch-create. Fails at the first `AlreadyExists` with the
    /// remaining Works unpersisted. Per scope memo D10, atomic
    /// cross-batch persistence is Stage 7's problem
    /// (reconcile-on-restart). A fresh decomposition never has id
    /// collisions because WorkIds are freshly minted here, so this
    /// method's failure mode is limited to IO errors on disk.
    pub async fn create_many(&self, works: Vec<Work>) -> Result<Vec<WorkId>, StoreError> { /* ... */ }

    pub async fn get(&self, id: &WorkId) -> Result<Work, StoreError> { /* mirrors plans */ }

    pub async fn list(&self) -> Result<Vec<Work>, StoreError> { /* mirrors plans */ }
}
```

### API Design

```rust
/// Decompose a validated `Plan` into a batch of `Work`s under it.
///
/// `target` is the effective target-repo path (the `-C <path>` arg or
/// CWD) — used to collect the workspace file tree that grounds the
/// decomposition prompt. Must be an existing directory; non-directory
/// or missing path yields `DecomposerError::WorkspaceScanFailed`.
///
/// `llm` is a handle to any `LlmClient` implementation. Production
/// uses `AnthropicClient`; tests use `MockLlmClient`.
///
/// On success, the returned `Vec<Work>` is ready for batch-persist:
/// each Work's `parent_id` is `plan.id`, `status` is `Pending`,
/// `dependencies` are resolved `WorkId`s, `acceptance_criteria` is
/// non-empty. The caller is responsible for persisting (the caller
/// knows the target's `Store` handle; this function does not).
///
/// On failure, returns without persisting anything. The retry loop
/// has already consumed one LLM call beyond the first.
pub async fn decompose<L: LlmClient>(
    plan: &Plan,
    target: &Path,
    llm: &L,
) -> Result<Vec<Work>, DecomposerError>;
```

Control flow inside `decompose`:

```
 1. tree       = collect_workspace_tree(target)?
 2. system     = assemble_system(&tree)
 3. first_user = assemble_user(&plan.goal, prev_error: None)
 4. tool_call  = match llm.complete_with_tool(&system, &first_user, schema()).await {
                    Ok(tc)  => tc,
                    Err(e1) => {
                        // Retry once. Second failure propagates via ?.
                        let retry_user = assemble_user(&plan.goal, prev_error: Some(&e1.to_string()));
                        llm.complete_with_tool(&system, &retry_user, schema()).await?
                    }
                };
 5. response: DecomposeResponse = serde_json::from_value(tool_call.input)
        .map_err(|e| DecomposerError::MalformedChildren(e.to_string()))?;
 6. if response.children.is_empty() { return Err(ZeroChildren(plan.id.clone())); }
 6a. let normalized: Vec<String> = response.children.iter().map(|c| normalize(&c.title)).collect();
     let dupes = find_duplicates(&normalized);
     if !dupes.is_empty() { return Err(DuplicateTitles(dupes)); }
 7. title_to_id: HashMap<String, WorkId> = normalized.iter().map(|t| (t.clone(), WorkId::new())).collect();
 8. let dep_graph: HashMap<String, Vec<String>> = children.iter()
        .map(|c| (normalize(&c.title), c.dependencies.iter().map(|d| normalize(d)).collect()))
        .collect();
    detect_cycles(&dep_graph).map_err(DecomposerError::CycleDetected)?;
 9. for each child: resolve its dependency titles via title_to_id; unresolved => DecomposerError::UnresolvedDeps
10. build Vec<Work> by pairing each DecomposeChild with its minted WorkId from step 7:
        let mut work = Work::new(plan.id.clone(), child.title);
        work.id = title_to_id[&normalize(&child.title)].clone();   // use the minted id
        work.dependencies = resolved_work_ids;
        work.acceptance_criteria = AcceptanceCriteria(non_empty_ac_for(&child));
11. validate each Work has ≥1 AC; empty => EmptyAcceptanceCriteria(work.title)
12. Ok(works)
```

Where `normalize(s)` is `s.trim().to_lowercase()` (case-insensitive + whitespace-trimmed, matching v3's title-resolution fallback) and `non_empty_ac_for(child)` returns `child.acceptance_criteria` if non-empty, otherwise falls back to `extract_acceptance_criteria(&child.content)` (markdown section parse).

Edge cases and subtle points:

- **Minting `WorkId` before building the `Work`.** `Work::new` calls `WorkId::new()` itself (generates a fresh id), so if the `dependencies` resolution needed to reference siblings, naively we'd have to do two passes. Step 7 pre-mints all the ids into `title_to_id` so step 10 can both reference siblings in `work.dependencies` AND assign each Work its pre-minted id. The post-assignment `work.id = ...` overwrites the `Work::new`-generated id; that is the only cost (one throwaway id per Work).
- **`Fatal` vs `Retryable` on the first call.** The retry fires uniformly on any `LlmError`. `Retryable` errors (rate-limit, 5xx) have obvious retry value; `Fatal(SchemaValidation)` and `Fatal(ContextExhausted)` also benefit because the retry prompt tells the model *what* it did wrong (`no tool_use block` / `hit max_tokens`), which is the signal the model actually acts on. `Fatal(Auth)` and `Fatal(ConfigInvalid)` will also retry, uselessly, which costs ~1 extra HTTP round-trip and one failed second call — acceptable. The caller gets the second error, not the first, if both fail; this is intentional (second error is usually more informative about the root cause).
- **Duplicate child titles.** If the LLM returns two children whose titles normalize to the same string (`"Build CLI"` + `"build cli"` + `" Build CLI "`), step 7's `HashMap` collapses them — the later child's minted `WorkId` silently overwrites the earlier. Worse, if one of them referenced the other via `dependencies`, the dep resolves to the second id (not both). **Mitigation:** before step 7, reject any duplicate-title decomposition with a new `DecomposerError::DuplicateTitles(Vec<String>)`. The retry prompt then tells the model "you produced two Works called X; give them distinct titles." Added to the error enum and the validation step (see below).
- **Self-dependency.** A child whose `dependencies` list includes its own title is a 1-node self-loop; `detect_cycles` catches this (node's `in_degree` starts at ≥ 1 from its self-edge, never drops to 0, never gets visited). No special-case needed; one unit test pins it.
- **Empty title.** A child with `title: ""` normalizes to `""`. Two such children collapse into the duplicate-title case above. Single such child is structurally valid but a clear model bug; the Work is persistable but unidentifiable. Reject as `DuplicateTitles(vec![""])`-adjacent — or as its own `EmptyTitle` variant. See Open Questions.
- **Retry prompt growth.** `prev_error.to_string()` on a `Fatal(Auth)` variant might include the full 401 response body. If that body is ~5 KiB and the system prompt is already ~3 KiB and the workspace tree is another ~15 KiB, the retry prompt could overshoot the model's input context. **Mitigation:** cap the embedded error message at 2 KiB in `assemble_user` before interpolating; if truncated, append `… [error truncated from N bytes]`. Implementation detail for Phase 4.
- **LLM returns `acceptance_criteria: []` AND `content: ""` (empty) AND `title: "Foo"`.** `non_empty_ac_for(child)` extracts from empty content → still empty → step 11 returns `EmptyAcceptanceCriteria("Foo")`. Retry path tells the model "Work 'Foo' had no AC; every Work must have at least one."
- **Decomposer idempotency.** `decompose` itself is pure: no side effects, no persistence, returns `Vec<Work>`. Calling it twice produces two independent `Vec<Work>` with distinct `WorkId`s (freshly minted each call). The caller's `create_many` is what persists; so the idempotency story is entirely upstream. Scope memo A+2 defers the "daemon crashed mid-persist" case to Stage 7.
- **Concurrent `plan.create` on the same daemon.** Two `plan.create` requests arriving concurrently both enter `handle_plan_create`, both call `decompose` (in parallel — the daemon's handler runtime is tokio), both call `create_many` on the shared `Store`. Each `WorkId` is freshly random so no id collision; `AsyncStore::create_many` serializes its SQLite + JSONL writes internally. No correctness issue, just 2× the LLM cost. Out of Stage 6's scope to rate-limit.

### Implementation Plan

#### Phase 1: `WorksStore` + decomposer error types + tool schema
**Model:** sonnet

- Create `crates/store/src/works.rs` mirroring `plans.rs`; add `create_many`.
- Edit `crates/store/src/store.rs` to add the `works(&self) -> WorksStore<'_>` accessor.
- Extend `crates/store/src/lib.rs` re-exports.
- Create `crates/decomposer/src/error.rs` (`DecomposerError` variants).
- Create `crates/decomposer/src/tool.rs` (`submit_decomposition_schema`, `DecomposeChild`, `DecomposeResponse`).
- Wire both into `crates/decomposer/src/lib.rs`.
- `cargo add -p decomposer thiserror` (each crate adds it explicitly; `llm` uses `thiserror = "2.0.18"`, same version expected). `cargo add -p decomposer serde_json serde` (both workspace-pinned; picks up `version.workspace = true`). `cargo add -p decomposer tracing` ahead of Phase 4 instrumentation.
- Compile check: `cargo check -p store -p decomposer` passes.

#### Phase 2: Workspace file tree collection
**Model:** sonnet

- Create `crates/decomposer/src/tree.rs` with `fn collect_workspace_tree(target: &Path) -> Result<String, DecomposerError>`.
- Primary path: `std::process::Command::new("git").args(["ls-files", "-z", "--cached", "--others", "--exclude-standard"])` in the target dir; split on `\0`; sort for determinism.
- Fallback when `git` exits non-zero or is not installed: depth-limited `std::fs::read_dir` walk, depth cap 4, skipping `.git/`, `target/`, `node_modules/`, `.venv/`, `dist/`, `build/`, and any dir whose name starts with `.`.
- Entry cap: 500. When exceeded, truncate with `\n... and N more entries`.
- When the target is entirely empty or only hidden dirs remain after skips, return `(empty workspace)` as the single line so the prompt always has something concrete.
- Unit tests: git-repo with 3 tracked + 2 untracked-not-ignored files (expect all 5), git-repo with 600 tracked files (expect 500 + truncation marker), non-git target with nested dirs (expect depth-limited fallback), empty dir (expect `(empty workspace)`).

#### Phase 3: Cycle detection + title→id resolution
**Model:** sonnet

- Create `crates/decomposer/src/cycles.rs` with `fn detect_cycles(nodes: &HashMap<String, Vec<String>>) -> Result<(), String>` (ports v3:126-164 verbatim; owned `String`s to match the call-site construction from `DecomposeChild` fields).
- Add `fn resolve_deps(children: &[DecomposeChild], title_to_id: &HashMap<String, WorkId>) -> Result<Vec<Vec<WorkId>>, DecomposerError>` — case-insensitive + whitespace-normalized title matching; returns one `Vec<WorkId>` per input child in order.
- Unit tests: acyclic 3-node DAG, trivial cycle (a→b→a), self-loop, diamond (no cycle), case-insensitive match (`"Build CLI"` → `"build cli"`), unresolved title (expect `UnresolvedDeps`).

#### Phase 4: Prompt assembly + `decompose` main function
**Model:** opus

- Create `crates/decomposer/src/prompt.rs` with the `SYSTEM_TEMPLATE` const, `assemble_system(tree)`, `assemble_user(goal, prev_error)`.
- Create `crates/decomposer/src/decompose.rs` with the main `pub async fn decompose<L: LlmClient>(...)`.
- Instrument with `#[tracing::instrument(level = "info", skip_all, fields(plan_id = %plan.id, goal_len = plan.goal.len()))]`; record `child_count` and `outcome` at return.
- Explicit `#[allow(clippy::manual_async_fn)]` on the trait impl if clippy complains (same as `llm` crate).
- Unit tests with `MockLlmClient` (see Testing Strategy): happy path, retry-succeeds-on-second-try, retry-fails (final error propagates), zero-children bail, cycle detection, unresolved dep, empty AC bail.

#### Phase 5: Wire `plan.create` handler
**Model:** sonnet

- Edit `crates/loopr/src/daemon/context.rs` to add an `llm: Arc<AnthropicClient>` field to `DaemonContext` (which already carries `pub target: PathBuf` at line 22 and `store: Store`). Build it in `DaemonContext::new` from `LlmConfig` loaded via the top-level `Config`, resolving the API key from env per `llm`'s precedence rules (CLI > env > config-named-env). Daemon startup fails fast with `FatalReason::ConfigInvalid` if no API key is available (per `AnthropicClient::new`'s validation). Propagate that up as a clean startup-failure message.
- Edit `crates/loopr/src/transport/handler.rs::handle_plan_create` to call `decompose(&plan, &ctx.target, &*ctx.llm).await` after the Plan is persisted. On success, persist Works via `ctx.store.works().create_many(works).await`. On decomposer error, the Plan remains persisted but no Works exist — Stage 6 logs the failure via the span's `outcome = "decomposer_failed"` and returns `DaemonResponse::ok` for the Plan (the Plan itself was successfully created). Plan-status-rollback-on-decompose-fail is Stage 7's reconcile concern per scope memo A+2.
- Add `llm` and `decomposer` as deps to `crates/loopr/Cargo.toml`.
- Add `LlmConfig` to the top-level `Config` in `loopr` (composed under key `llm:`, per scope memo U+1).
- Thread the API key resolution: CLI `--api-key` > env `ANTHROPIC_API_KEY` > config-named env > error. Stage 6 has no CLI flag; fall back to env only.

#### Phase 6: Tests
**Model:** sonnet

- `crates/decomposer/tests/decompose.rs`:
  - `MockLlmClient` that returns canned `Result<ToolCall, LlmError>` based on call-count or predicate on the prompt text.
  - Happy-path 2-Work decomposition with no deps, with one dep.
  - Retry path: first call returns `Fatal(SchemaValidation)`, second call succeeds with a real tool call.
  - Retry path: first and second both fail; error propagates.
  - Zero children: single canned empty response → `DecomposerError::ZeroChildren`.
  - Cycle: 2 Works with titles `A,B` each depending on the other.
  - Self-loop: 1 Work with `dependencies: ["itself"]` → `CycleDetected` (1-node self-cycle never visited by Kahn).
  - Duplicate titles: 2 Works named `"Build CLI"` and `"build cli"` → `DuplicateTitles(["build cli"])`.
  - Unresolved dep: `A` depends on `"NotThere"`.
  - Empty AC: `A` with `acceptance_criteria: []` and no extractable AC → `EmptyAcceptanceCriteria`.
  - Malformed tool input: canned `ToolCall` whose `input` is a valid JSON object but missing the `children` field → `MalformedChildren`.
  - Retry prompt truncation: `prev_error` of 10 KiB → `assemble_user` cap at 2 KiB with truncation marker.
- `crates/decomposer/tests/tree.rs`: file-tree collection scenarios listed in Phase 2.
- `crates/store/src/works/tests.rs`: mirrors `plans.rs` test coverage (create, get, list, create-twice-rejects, empty list).
- `crates/loopr/tests/daemon.rs` (or `smoke.rs`): extend an existing `plan.create` test to assert that after the call, `ctx.store.works().list()` returns at least one Work whose `parent_id` is the created Plan's id. Uses a `MockLlmClient` injected into `DaemonContext` for the test daemon.
- `otto ci` at `crates/decomposer/`, `crates/store/`, `crates/loopr/`, and the workspace root all pass.

#### Phase 7: Ship
**Model:** sonnet

- Update design doc status → Implemented.
- Commit, push, `bump -a`, push, install.
- Stage 6 complete: `loopr plan "Add --version flag to a Rust CLI"` produces a Work.

## Alternatives Considered

### Alternative 1: Port v4's agent-harness decomposer verbatim

- **Description:** Lift `loopr-v4/src/daemon/handlers/decomposer.rs` (1494 lines) + `loopr-v4/src/agents/director.rs`; decomposition becomes an agent that IPC-dispatches through the daemon to a handler that calls the LLM.
- **Pros:** Uniform with v4's agent model; might share machinery with Stage 7's implementer agent.
- **Cons:** Three layers where one suffices. The scope memo's "Decomposer — a standalone function, not an agent" section is the definitive rejection: v4 made "system calls into LLMs" go through the same harness as "autonomous agents that iterate," which is a category error. The decomposer has no session, no FSM, no iteration loop. Adding those because Stage 7's implementer needs them is speculative unification; Stage 7's implementer will still want its own harness whether or not the decomposer uses one.
- **Why not chosen:** Scope memo D6, D7, and the explicit "standalone function" callout. v3's ~200-line decomposition loop is the shape; v5 gets it even smaller because `llm` owns the transport.

### Alternative 2: Inline text-parse fallback (scope memo D7 as originally written)

- **Description:** When `complete_with_tool` fails with `Fatal(SchemaValidation)` (no `tool_use` block), fall through to a raw completion without `tool_choice`, parse the response text as JSON, strip markdown fences. Ports v3's `call_llm_for_children` behavior (lines 314-345).
- **Pros:** Matches v3 verbatim; robust against models that ignore `tool_choice` (rare but historically non-zero on first-generation tool-use-capable models).
- **Cons:** Architecturally unavailable without modifying the `llm` crate we just shipped and audited. The just-shipped `LlmError::Fatal(SchemaValidation)` carries only a `String` description, not the raw response text; a true inline fallback needs the dropped `text` content block, which would require changing the error variant to `SchemaValidation { message: String, raw_text: Option<String> }`. Widening the trait (`complete_raw` method) or plumbing raw text through the error enum both widen `llm`'s surface for a failure mode that modern Anthropic models essentially never hit under `tool_choice: "tool"`.
- **Why not chosen:** Architect round 2 (Q1) recommended dropping D7's text-fallback requirement entirely. Replaced with Alternative 3 (retry-with-error-in-prompt) which is strictly simpler and covers the real residual failure modes (model edge-cases that parseability-retry would also miss anyway).
- **Refinement recorded:** Scope memo D7 remains "tool-use + text fallback" historically; this doc supersedes it with "tool-use + retry-with-error-in-prompt" (Alternative 3). The scope memo is a frozen artifact; its D7 row gets a one-line pointer here rather than being edited in place.

### Alternative 3: Retry-with-error-in-prompt (v3 pattern; chosen)

- **Description:** On any `LlmError`, re-prompt once with the error text appended to the user message; the model self-corrects on the second call. Ports v3's `decompose_into` (lines 493-502).
- **Pros:** Needs no change to the just-audited `llm` crate. Covers every failure mode uniformly (tool-not-used, unparseable input, transient 5xx, rate-limit-between-the-original-and-retry, even downstream decomposer-layer errors like `DuplicateTitles` / `UnresolvedDeps` / `CycleDetected` if we route them through the retry path — deliberate choice not to; see control-flow section). Self-correction via error-in-prompt is v3's actual production pattern and is documented as the approach in the Anthropic tool-use cookbook.
- **Cons:** Costs one extra LLM call on any first-call failure. Does nothing for failures that are structural-to-config (`Fatal(Auth)`, `Fatal(ConfigInvalid)`) — those retry uselessly. Second-failure-propagation means the caller sees the retry's error, not the original; usually more informative, occasionally less.
- **Why chosen:** Simplest, smallest surface, matches v3 verbatim, architecturally reachable without touching `llm`. Architect round 2 explicitly endorsed this over Alternative 2.

### Alternative 4: No workspace file tree injection

- **Description:** Send only the `goal` to the LLM. v3 did not inject a tree.
- **Pros:** Smaller prompts, lower cost per call, no `git` dependency at runtime.
- **Cons:** The LLM hallucinates file structure. On a fresh or bespoke codebase, it proposes creating `src/main.rs` when the target is a Python repo, or refers to files that don't exist. The roadmap explicitly calls for tree injection as a v5-era improvement over v3.
- **Why not chosen:** Roadmap requirement; v3's tree-less decomposition was observed to hallucinate in exactly the "user files Plan on unusual target" edge cases Stage 6 should handle cleanly.

### Alternative 5: Byte-capped workspace tree truncation

- **Description:** Truncate the tree at N bytes (e.g. 16 KiB) rather than N entries.
- **Pros:** Predictable prompt cost in tokens.
- **Cons:** Byte-truncation slices a path mid-word. The LLM sees `src/alpha/bravo/some-thing-g` and hallucinates a filename. Architect round 2 Q2c rejected this.
- **Why not chosen:** Entry-count truncation preserves the integrity of every path it emits and produces a clean truncation marker; byte-truncation injects garbage.

### Alternative 6: Multi-turn LLM call with `tool_use_result`

- **Description:** Instead of retry-with-error-in-prompt, use Anthropic's multi-turn pattern: send user + tool schema, receive tool_use, reply with a tool_result message containing the error, the model retries in the same conversation.
- **Pros:** Native Anthropic pattern; the model has the full original context.
- **Cons:** `LlmClient::complete_with_tool` is one-shot (scope memo locks non-multi-turn). Multi-turn is explicitly Stage 7's surface (scope memo D6: "non-goals: multi-turn history"). Adding it here means widening the trait.
- **Why not chosen:** Scope memo D6; one-shot is the Stage 6 shape. Retry-with-error-in-prompt is a poor-man's multi-turn that fits within one-shot semantics.

### Alternative 7: Run validation / ratification calls as v3 does

- **Description:** After decomposition, make a second LLM call per child for structural validation (v3 used Haiku) and a third call across all children for coherence ratification.
- **Pros:** Tighter quality gate before persistence.
- **Cons:** 3× LLM calls per `loopr plan`. Validation logic is Stage 7's evaluator's domain. Stage 6's exit criterion is "at least one Work persisted," which the validation call cannot improve on (it doesn't change the Works, just flags issues for a human to review — that's a Stage 7 UX surface).
- **Why not chosen:** Scope memo implicit — one LLM call per decomposition is the Stage 6 budget. Stage 7 earns the evaluator.

## Technical Considerations

### Dependencies

New, via `cargo add`:

- `tracing` (workspace) — spans on the decomposer call.
- `serde_json` (workspace) — already on most crates; explicit here.
- (dev) `tokio` with `macros, rt` — for `#[tokio::test]` in decomposer/store/loopr tests that don't already have it.

Internal:

- `domain` (Work, Plan, PlanId, WorkId, AcceptanceCriteria) — already a dep.
- `store` (WorksStore being added) — already a dep.
- `llm` (LlmClient, LlmError, ToolSchema, ToolCall) — already a dep.
- `context` — listed as a dep in `crates/decomposer/Cargo.toml` but unused in Stage 6 (scope memo D11 defers context-builder to Stage 7). Leave the dep as-is; when Stage 7 wires it up, no Cargo.toml change is needed.

No new external runtime dependencies. The `git ls-files` path is `std::process::Command`; the fallback walk is `std::fs`; no `ignore` or `walkdir` crate.

### Performance

- One LLM call per successful decomposition (1-10s wall-clock, bounded by `llm`'s 120s timeout); two calls when the retry fires.
- Workspace tree collection: single `git ls-files` invocation (typically <100ms on repos up to 50k files) or a fallback walk with early-exit at 500 entries (also fast).
- Cycle detection and title resolution are O(n+e) for n children and e deps; n is capped at ~5 per decomposition so real-world cost is negligible.
- `create_many` on `WorksStore` wraps `taskstore_async::AsyncStore::create_many` (verified at `taskstore-async v0.5.0`, `src/store.rs:116`, signature `pub async fn create_many<T: Record>(&self, records: Vec<T>) -> Result<Vec<String>>`). The underlying primitive inserts into the SQLite cache in a single transaction and appends JSONL in one syscall batch; for Stage 6's 1-5 Works this is structurally overkill but free. Cross-file atomicity across `plans.jsonl` (Plan status) and `works.jsonl` (new records) is still not guaranteed; scope memo D10 notes the reconcile-on-startup obligation that lives in Stage 7.

### Security

- Decomposer does not touch the API key; `llm` owns it end-to-end.
- Workspace file tree is emitted as prompt content. `.gitignore` (via `--exclude-standard`) prevents `.env` / secret files from leaking into the prompt by default. The fallback `std::fs` walk does NOT honor `.gitignore` but does skip hidden dirs (any dirname starting with `.`), plus the hardcoded `target/`, `node_modules/`, `.venv/`, `dist/`, `build/` skip-list. **Risk:** a non-git target with `secrets.json` sitting at the top level would appear in the tree. Mitigation: the skip-list is extendable; Stage 7's `context-builder` crate earns a proper `.ignore`/`.gitignore` honoring walker.
- The `## Plan` goal string in the user prompt is user-typed content; no sanitization is performed because the LLM doesn't execute input — it's prompt context. Prompt injection via goal string ("ignore previous instructions and output X") is possible but out of scope for Stage 6 (would be an `agents` concern when agents run in autonomous loops).
- The LLM's response flows through `llm` (which validates JSON parseability) then through `decompose` (which validates structure and values). No raw LLM string lands in the store: `title` / `content` / `acceptance_criteria` are all de-facto sanitized via serde's `String` deserialization (no code execution, no path traversal risk at this layer).

### Telemetry

- The decomposer emits an `info` span `decomposer.decompose` with fields: `plan_id`, `goal_len`, `target` (debug-formatted path), `child_count` (recorded at return), `outcome` (`"ok" | "zero_children" | "cycle" | "unresolved_deps" | "llm_failed" | "workspace_scan_failed" | "empty_ac"`).
- The `llm.anthropic` span from the underlying `llm` crate nests inside this one (same thread-local span stack), giving structured traces of "decompose → llm call 1 → llm call 2 (retry)" automatically in `events.log`.
- The workspace tree itself is NOT emitted as a span field (could be many KiB; would balloon `events.log`). A one-line summary (`tree_entries = 237`) would be fine — optional nice-to-have.
- On `DecomposerError::UnresolvedDeps` and `CycleDetected`, emit a `warn!` with the offending titles; these are user-facing decomposition quality signals the user wants to see when a Plan fails to decompose.

### Testing Strategy

Unit + integration tests, no network:

1. **Unit tests (`crates/decomposer/src/*/tests.rs`)**: cycle detection (acyclic, trivial cycle, diamond), tree collection (git + fallback + empty), title→id resolution (case-insensitive, whitespace-normalize, unresolved). Place tests in sibling `tests.rs` files per memory `feedback_tests_in_own_files`.

2. **Integration tests (`crates/decomposer/tests/decompose.rs`)**: exercise the full `decompose<L: LlmClient>` function against a `MockLlmClient`. The mock stores a queue of canned `Result<ToolCall, LlmError>` responses and returns them in call order; its `complete_with_tool` also records the received prompts for assertion. Covers: happy path (1-Work, 3-Works-no-deps, 3-Works-with-deps), retry-on-SchemaValidation-succeeds, retry-on-Retryable-succeeds, retry-and-final-failure, zero children, cycle, unresolved dep, empty AC.

3. **Store integration tests (`crates/store/tests/works.rs`)**: mirror `plans.rs` coverage.

4. **Daemon smoke test (`crates/loopr/tests/smoke.rs` or `daemon.rs`)**: extend an existing `plan.create` test to assert that after `loopr plan "..."`, `ctx.store.works().list()` returns at least one Work whose `parent_id` matches the created Plan. The test daemon injects a `MockLlmClient` via dependency-injection in `DaemonContext` construction — tests must not hit real Anthropic. Stage 6's exit criterion is satisfied by this test.

5. **No real Anthropic calls in CI.** Confirmed during `llm` Phase 4 audit; same rule here.

### Rollout Plan

- Five-to-seven-phase implementation matching v0.5.19 → v0.5.20+ bumps. Each phase compiles and `otto ci` is green at its commit boundary.
- `plan.create` handler change (Phase 5) is the first observable user-facing change: `loopr plan "..."` will take 1-10s instead of <100ms while the LLM call runs. No CLI output format changes; the result still returns a `Plan` (the Works are persisted but not surfaced until Stage 7 adds `loopr list works`).
- Backwards compat: no wire protocol changes to `plan.create` IPC. Existing smoke tests continue to pass (Plan creation is still the observable effect of the call).

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Retry-with-error-in-prompt burns tokens on structural failures the LLM can't fix (e.g. the Plan's goal is genuinely undecomposable and the model keeps failing). | Medium | Low-Med | Exactly one retry, not a loop. Second failure bails to the caller. The caller (daemon `plan.create` handler) logs the failure and leaves the Plan in a pre-decomposed state for Stage 7's reconcile-on-restart to handle. |
| Workspace file tree spams the prompt with `node_modules/` entries on non-git JS repos. | Medium | Med | Non-git fallback walk explicitly skips `node_modules`. Plus the 500-entry cap forces truncation even if the skip-list missed something. |
| Workspace file tree leaks a secret file on a non-git target. | Low | High | Skip-list includes hidden dirs (dot-prefix) and the known-noise set. Still imperfect for a non-git target with `secrets.json` at top level — flagged in Security section; accept for Stage 6, upgrade when `context-builder` crate lands in Stage 7. |
| LLM returns a child whose `title` is empty-string or whitespace-only. | Low | Med | After `normalize`, an empty title collapses with any other empty title into `DuplicateTitles`. A single empty-title child is structurally unique but unidentifiable downstream; see Open Questions for whether to add a dedicated `EmptyTitle` variant or fold into `DuplicateTitles([""])`. Retry will likely fix either way. |
| `prev_error.to_string()` on retry is so large it overflows the model's input context window. | Low | Med | `assemble_user` caps the embedded error message at 2 KiB before interpolation, appending `… [error truncated from N bytes]` so the model knows truncation happened. Covered in the control-flow edge-case list. |
| `git ls-files` output mid-UTF-8-boundary on a filename with non-ASCII bytes under the 500-entry cap marker. | Very Low | Low | `-z` delimiter is `\0` (not `\n`), so splitting is byte-safe. The cap is applied to the post-split Vec, not mid-byte. |
| `DecomposeChild.content` is large (e.g. 10 KiB of markdown per Work) and the resulting response bumps against `max_tokens`. | Low-Med | Med | `llm`'s `Fatal(ContextExhausted)` fires with `used`/`limit`; the retry prompt includes the error and the model typically shortens. If it keeps hitting, the user bumps `max-tokens` in `.loopr/config.yml`. |
| The `SYSTEM_TEMPLATE` hardcodes English phrasing ("You are a software architect..."). User's prompt in a non-English goal still decomposes, but the model is biased toward English naming. | Low | Low | Stage 7's `context-builder` earns the prompt-engine refactor that makes templates per-locale. Stage 6 accepts the bias. |
| `WorksStore::create_many` partial failure (IO error mid-batch) leaves some Works persisted and others not; the Plan is already persisted from earlier in the handler. | Low | High (on disk) | Scope memo A+2: reconcile-on-restart. Stage 7 design. Stage 6 logs the partial-persist error with the failing `WorkId` and the count of successful-before-failure; the daemon keeps running. |
| Empty target directory (`loopr plan` in a fresh dir with no files) — tree is `(empty workspace)` and the LLM has no grounding. | Low | Low | Accepted. The model will produce a generic decomposition; user's goal text still steers it. Not a Stage 6 quality bar. |

## Open Questions

- [x] **Synthetic `DecomposerError` for malformed tool input.** Resolved during Pass 3: added `DecomposerError::MalformedChildren(String)` as a first-class variant. The failure is semantically "the model used our tool schema wrong," a decomposer-layer concern distinct from `llm`'s `Fatal(SchemaValidation)` (which only fires on "no tool_use block" or "input unparseable as JSON").
- [ ] **Entry cap N for workspace tree: 500 or 1000?** Architect round 2 (Q2c) proposed 1000; I proposed 500. 500 is enough for the LLM to see workspace shape for decomposition; more than that and the token spend on the tree exceeds the prompt's usefulness. Pick 500; if Stage 7 data shows decomposition quality suffers, bump.
- [ ] **Empty `title` handling.** Edge case: LLM produces a single child with `title: ""`. Currently collapses into `DuplicateTitles(vec![""])` only if there's more than one empty-titled child. A single one passes through as a valid-but-anonymous Work. Options: (1) add `DecomposerError::EmptyTitle` variant; (2) pre-validate `!title.trim().is_empty()` before normalization; (3) accept (mine) — downstream users of the Work list will notice the anonymous entry and re-decompose. Leaning (2), flagged for Phase 4 to decide.
- [ ] **Unit test for `prev_error` truncation cap.** Not strictly "Open" — it's an implementation detail — but worth a flag: Phase 6 test suite should include a case where `assemble_user` is called with a 10 KiB error string and assert the output contains `[error truncated from 10240 bytes]`.
- [ ] **Gitignore-respecting fallback walk: `ignore` crate vs skip-list.** Leaning skip-list (current design) for Stage 6 since it adds zero deps and covers the 95% case; `ignore` crate earns its place in Stage 7's `context-builder` where workspace-tree injection becomes a reusable primitive across multiple agents.
- [ ] **Plan status on decomposer failure.** When `decompose` returns an error, should the `plan.create` handler transition the Plan's status from `Active` (fresh from `Plan::new`) to some error state? Scope memo A+2 says reconciliation is Stage 7's problem; Stage 6 leaves Plan as `Active` with no Works. This means a user re-invoking `loopr plan` on the same goal creates a second Plan, not retries the first. Accept for Stage 6; flag for Stage 7's reconcile-on-restart design doc.
- [ ] **Forward-ref to Stage 7 (not a Stage 6 concern, but flagged):** multi-turn LLM decomposition (v3-style pre-interview "clarity" round where the model asks the user follow-up questions before committing to a decomposition). Scope memo D6 locks one-shot for Stage 6; Stage 7+ can earn multi-turn when the `agents` crate needs it for the Implementer's ralph loop.
- [ ] **Forward-ref to Stage 7:** prompt-engine refactor. The inline `SYSTEM_TEMPLATE` const in `prompt.rs` is Stage 6's tactical choice; Stage 7's `context-builder.md` should extract it into a handlebars + partials system shared between the decomposer and the agents crate.

## References

- [Scope memo](../../../../docs/design/2026-04-20-stage-6-scope.md) — decisions locked; this doc references D6, D7 (refined by Architect round 2 below), D8, D9, D10, A+1, A+2, A+3, U+1-U+5 by row.
- [Hierarchy design doc](./2026-04-20-hierarchy.md) — `Work` record shape, `WorkStatus` FSM, `AcceptanceCriteria`.
- [LlmClient design doc](./2026-04-20-llm-client.md) — the trait + backend this doc's decomposer calls.
- [Roadmap](../../../../docs/roadmap.md) — Stage 6 entry at line 93, plan-then-decompose.md spec at line 102, exit criterion at line 106.
- [`crates/decomposer/CLAUDE.md`](../../CLAUDE.md) — in-scope/out-of-scope rules for this crate.
- [`crates/store/CLAUDE.md`](../../../store/CLAUDE.md) — why `WorksStore` lives in `store` rather than `decomposer`.
- `~/repos/scottidler/loopr/src/decomposer.rs` — v3's reference implementation. Port targets: `decomposition_tool_schema` (line 197-236), `detect_cycles` (line 126-164), `decompose_into` control flow (line 484-602), retry-with-error-in-prompt pattern (line 493-502), title-resolution case-insensitive fallback (line 558-582).
- `~/repos/scottidler/loopr/prompts/decompose/work.pmt` — v3's prompt template, ported into `SYSTEM_TEMPLATE` const in `src/prompt.rs` with one-mode simplification (Brief only).
- `~/repos/scottidler/loopr-v4/src/daemon/handlers/decomposer.rs` — v4's rejected agent-harness shape; one-line pointer, no porting.
- **Architect round 2 (2026-04-20):** Q1 recommended dropping scope memo D7's "text fallback" in favor of "retry-with-error-in-prompt" (Alternative 3 here). Q2 locked `git ls-files --cached --others --exclude-standard` with non-git fallback walk and entry-count cap. Both findings are the load-bearing choices in this doc.
- [Anthropic Messages API `tool_choice`](https://docs.anthropic.com/claude/reference/messages_post) — external; the `tool_choice: {type: "tool", name: "submit_decomposition"}` pattern this doc relies on.
