# Design Document: Strategy-to-Primitive Wiring Integrity

**Author:** Scott Idler
**Date:** 2026-04-17
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Three latent defects at the strategy-YAML -> primitive -> IPC-handler
boundary are blocking end-to-end runs. None produce a compile error, none
fail a unit test, all three surface as tight retry loops at first contact
with a real plan. This document closes the gaps by (1) bringing the IPC
wire format in line with the project-wide kebab-case convention (removing
the one layer that drifted), (2) adding startup-time strategy validation so
the next drift is caught at daemon boot instead of engine tick, and (3)
making `create-integration-branch` self-idempotent so repeat invocations
are safe.

## Problem Statement

### Background

Loopr's engine is driven by declarative YAML strategies. Each strategy is a
trigger condition plus a list of primitive invocations with named parameters.
Primitives are Rust structs that implement the `Primitive` trait; most of them
forward their invocation into the daemon via an IPC bridge request. The IPC
handlers live inside the same daemon process and are the final arbiter of what
each domain action does (create agent, transition bundle, create branch, etc.).

This produces three layers of parameter naming, and today they disagree:

1. **Strategy YAML**: author-facing, kebab-case by convention (`role`,
   `target-id`, `plan-id`). This is the canonical form per
   `rules/general.md` (lowercase-hyphenated everywhere).
2. **Primitive `input_schema`**: declared on each Primitive impl; also
   kebab-case.
3. **IPC handler**: defined inside `src/daemon/handlers/`; snake_case
   (`agent_type`, `work_id`, `plan_id`, `bundle_id`). This is the one layer
   that drifted from the convention.

Most primitives forward the raw strategy params into the bridge request
verbatim. That only works when the names happen to line up. For `spawn-agent`
they do not line up, and the strategy dies on the first tick.

This document resolves the drift by aligning the IPC handler layer with
the project-wide kebab-case convention, rather than adding a translation
step to paper over the mismatch.

### Problem

Running `/e2e rust-version` against the v4 daemon surfaced two tight failure
loops inside the first second of plan execution and a third underlying cause
that the investigation uncovered:

**Defect 1: IPC handlers read snake_case keys while strategy YAML and
primitives produce kebab-case keys.**

- Strategy `decompose-plan` in `resources/decompose/strategies/default.yml:6`
  sends `role: decomposer` and `target-id: $trigger.scope-id` to
  the `spawn-agent` primitive.
- `SpawnAgent::execute` at `src/primitive/catalog/agent.rs:27` passes `params`
  verbatim into `ctx.bridge.request("agent.start", params)`.
- `handle_agent_start` at `src/daemon/handlers/agent.rs:23` reads
  `params.get("agent_type")` and rejects the call with
  `RpcError::invalid_params("agent_type is required")`.

Two things are wrong at this seam:

- **Case drift.** The handler reads `agent_type` (snake); the wire format is
  `agent-type` (kebab, per convention). Even if the key name were right, the
  case is wrong.
- **Name drift.** The strategy uses `role` but the handler calls the same
  concept `agent_type`. Resolving this design doc picks the canonical name
  (`agent-type`, to match the existing handler semantics) and renames the
  strategy / primitive accordingly.

Every engine tick retries; every retry fails. Decomposition never starts.

**Defect 2: `create-integration-branch` declares `Idempotency::GuardRequired`
but the engine ignores the declaration and the strategy has no guard.**

- `CreateIntegrationBranch::idempotency()` returns `Idempotency::GuardRequired`
  (`src/primitive/catalog/integration.rs:687`).
- The primitive executes `git branch integration/<plan-id> main` with no
  pre-check.
- `create-integration-branch-on-plan-active` in
  `resources/engine/strategies/git-lifecycle.yml:4` has no `guard:` clause.
- A global grep across `src/engine/` for `GuardRequired` returns zero matches,
  so the engine never checks the declaration.

First tick creates the branch; every subsequent tick fails with
`fatal: a branch named 'integration/pl-ag4cj' already exists`.

**Defect 3: `Primitive::validate_params` is declared on the trait but never
called from the daemon/engine path.**

- `src/primitive/types.rs:120` implements a default `validate_params` that
  rejects strategies referencing primitives without their required params.
- The only caller is `src/engine/tests.rs:1497`.
- Daemon startup at `src/daemon.rs:311` calls `validate_cross_references`
  (trigger-reference integrity) but not `validate_params`.

If the daemon had called `validate_params` on every `(primitive, strategy)`
pair at startup it would have caught defect 1 before the engine ever ticked.

### Goals

- Make the daemon refuse to start if any loaded strategy references a
  primitive with missing required params, a guard-required primitive with no
  guard, or a param name that a primitive does not declare.
- Eliminate the silent naming drift between strategy YAML, primitive input
  schemas, and IPC handlers.
- Harden `create-integration-branch` so repeat invocations are safe.
- Do this without breaking any currently-passing `otto ci` test.

### Non-Goals

- **No new strategy or primitive semantics.** Idempotency classes,
  trigger composition, guard registry - all unchanged.
- **No refactor of the Director agent.** The Architect round 2 also flagged
  Phase 6 tool-blindness and Phase 3 tool-set gaps. Those are Director-internal
  and out of scope for this document.
- **No change to daemon-internal Rust field names.** Handler structs keep
  snake_case field names for Rust-idiomatic code. The change is to the
  *wire format* they accept: JSON keys arriving on the IPC socket are
  kebab-case.
- **No wholesale YAML rewrite.** Strategies keep their existing kebab-case
  keys. Only strategies that use names drifted from the handler's canonical
  name (e.g. `role` -> `agent-type`) get renamed.
- **No new persisted state.** All changes are wire-format and
  load-time-validation only.

## Proposed Solution

### Overview

Close the three gaps at three well-defined seams:

1. **Kebab-case at the IPC wire format.** IPC handlers stop reading
   snake_case JSON keys and start reading kebab-case. Internal Rust field
   names stay snake_case; the translation happens once, via serde, at the
   deserialization boundary on the handler side. Primitives forward strategy
   params verbatim - no per-primitive translation method, no boilerplate.
   Where a name has drifted semantically (the `role` vs `agent-type` case),
   the design picks the canonical name and renames the diverging side. The
   result: one convention (kebab-case) from YAML all the way to the last
   byte before it hits a Rust struct field.

2. **Startup-time strategy validation.** Daemon startup gains two new gates
   next to the existing `validate_cross_references`:
   - `validate_primitive_params` walks every action step and calls the
     primitive's `validate_params` against the strategy's declared params.
     Also rejects any param key that is not in the primitive's declared
     `input_schema`.
   - `validate_guard_required` rejects any strategy that references a
     `GuardRequired` primitive without at least one guard clause on the step.

3. **Self-idempotent `create-integration-branch`.** The primitive itself
   checks `git show-ref` before creating the branch and returns success if
   the branch already points at the expected base. This mirrors the graceful
   handling already present in `DeleteIntegrationBranch` at
   `src/primitive/catalog/integration.rs:811` for "branch not found".

### Architecture

Before:

```
 strategy YAML              primitive catalog             daemon handler
 (kebab-case)               (kebab-case schema)           (snake_case wire)
                                                          (snake_case field)

 role: decomposer    -->    input_schema:           -->   params.get("agent_type")
 target-id: $...             ["role", "target-id"]        params.get("work_id")
                            ctx.bridge.request(           -> ERROR: drift
                             "agent.start", params)
```

After:

```
 strategy YAML              primitive catalog             daemon handler
 (kebab-case)               (kebab-case schema)           (kebab-case wire)
                                                          (snake_case field)

 agent-type: decomposer --> input_schema:           -->   #[derive(Deserialize)]
 target-id: $...             ["agent-type",               #[serde(rename_all =
                              "target-id"]                 "kebab-case")]
                            ctx.bridge.request(           struct AgentStartParams
                             "agent.start", params)        { agent_type: AgentKind,
                                                            work_id:    Option<String>,
                                                            bundle_id:  Option<String>,
                                                            ... }
```

Load path at daemon startup:

```
load_strategies()
  -> validate_schema()                  (existing)
  -> validate_cross_references()        (existing)
  -> validate_primitive_params()        (new: required-params + unknown-params)
  -> validate_guard_required()          (new: GuardRequired => guard clause)
  -> start engine
```

Any error aborts startup before the engine ticks once. Warnings are logged
and startup proceeds.

### Data Model

No persisted types change. Two new types in `src/engine/schema.rs`:

```rust
pub enum StrategyValidationKind {
    MissingRequiredParam { primitive: String, param: String },
    UnknownParam { primitive: String, param: String },
    GuardRequiredWithoutGuard { primitive: String, step_index: usize },
}
```

One new handler-side convention: each IPC handler that currently reads raw
`params.get("foo_bar")` lookups gets a typed params struct with
`#[serde(rename_all = "kebab-case")]`:

```rust
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct AgentStartParams {
    agent_type: AgentKind,
    work_id: Option<String>,
    bundle_id: Option<String>,
    target_id: Option<String>,
    model: Option<String>,
    context_from: Option<String>,
    query: Option<String>,
}

pub(super) fn handle_agent_start(
    stores: &Arc<Stores>,
    event_tx: &broadcast::Sender<DaemonEvent>,
    worktree_mgr: &WorktreeManager,
    req: DaemonRequest,
) -> DaemonResponse {
    try_handler!(req.id, {
        let params: AgentStartParams = serde_json::from_value(req.params.clone())
            .map_err(|e| RpcError::invalid_params(&format!("{}", e)))?;
        // ... use params.agent_type etc.
    })
}
```

Rust field names stay snake_case (idiomatic for the language); serde
rename_all handles the wire format. No per-primitive `params_to_ipc` method,
no Primitive-trait change.

### API Design

**`validate_primitive_params`** runs at daemon startup. For each strategy,
for each action step, for each param the strategy declares:

- Look up the primitive by name; error if unknown (already handled elsewhere).
- Call `primitive.validate_params(&step.params)` to catch missing required
  params.
- Walk the strategy's param keys and error on any key not in
  `primitive.input_schema()`.

**`validate_guard_required`** runs at daemon startup. For each action step
whose primitive returns `Idempotency::GuardRequired`:

- Error if the step's `guard` field is `None`.
- The step's guard name must resolve against the `GuardConditionRegistry`
  (reuses the existing lookup; do not duplicate).

**IPC handler params structs** are the only new per-handler code. Each
currently-raw handler (roughly 30 of them based on the grep) converts its
`params.get("foo_bar")` lookups into typed deserialization against a
kebab-case params struct. Handlers that already deserialize into typed
structs (there are a few) only need the serde attribute added.

### Edge Cases

- **`on-failure:` blocks.** `StrategyDefinition.on_failure` holds a list of
  action steps using the same structure as `action`. Both validators walk
  both lists; the schema uses the same step type either way.
- **Runtime interpolation (`$trigger.scope-id`, `$step.0.session-id`).**
  The validator sees the literal token as the param value - a non-empty
  string, so presence checks pass. Type checks would fail here (a string
  token where an integer is declared), which is one reason name-only
  validation ships before type-aware validation. Output-reference tokens
  (`$step.N.field`) are semantically valid references to a prior step's
  output schema; the validator does not dereference them.
- **Self-idempotent primitive that previously declared `GuardRequired`.**
  After Phase 4, `create-integration-branch` is downgraded to `Idempotent`
  so `validate_guard_required` does not demand a guard clause on strategies
  that reference it. `GuardRequired` remains in the enum for primitives
  whose idempotency can only be expressed at the strategy layer (e.g.
  `spawn-agent` with `no-active-sessions`).
- **Empty strategy file / empty action list.** The schema loader already
  treats zero steps as valid; the new validators iterate an empty list and
  return no findings. No special case needed.
- **Strategy references an unknown primitive.** Caught by the existing
  `validate_cross_references`; the new validators only run on strategies
  whose primitives resolved.
- **Handler deserialize error messages.** Moving from
  `params.get("agent_type")` + a hand-written `"agent_type is required"`
  to `serde_json::from_value` produces technical serde messages like
  `"missing field 'agent-type' at line 1 column 42"`. This is a minor
  ergonomic regression at runtime but an improvement at daemon boot (the
  new startup gates catch the same issue with the strategy file path and
  step index in the error message, which is more useful than a field name
  alone). Preserving friendly messages on a case-by-case basis via a
  wrapper converter is optional and can be added post-Phase 3 if needed.
- **CLI / TUI / integration-test callers.** Every in-tree caller of an
  IPC handler must flip to kebab-case. `src/cli/dispatch.rs`,
  `src/tui/run/ipc.rs`, and the integration tests in `src/tests/` are
  known sites. Phase 3 includes a grep sweep to enumerate all callers
  per handler during each handler's conversion commit. There is no CLI
  client versioning concern: loopr is one binary: the `cli`, `tui`, and
  `daemon` subcommands all live in the same target. A user never runs
  an older CLI against a newer daemon; they run whatever they built.
- **Dynamic-payload IPCs.** Handlers for `record.create`, `record.update`,
  and similar pass-through-the-map methods accept arbitrary keys by
  design. `deny_unknown_fields` is incompatible with their contract.
  Phase 3 explicitly spares these: their params structs keep
  `HashMap<String, Value>` (or equivalent) for the dynamic portion.
  Closed-schema handlers get the strict attribute; dynamic ones do not.
- **Ghost params (primitive reads undeclared key).** A primitive that
  calls `params.get("secret-key")` without declaring `secret-key` in
  its `input_schema` escapes `validate_primitive_params`. Caught by the
  schema coverage lint in Phase 1 (scan each primitive's source for
  `params.get("X")` literals, assert X is in `input_schema`).
- **External (non-loopr) IPC clients.** Today none exist - the IPC socket
  is loopr-internal. If a third party ever speaks the wire format, the
  breaking change to kebab-case is documented via the handler struct
  definitions themselves.

### Implementation Plan

#### Phase 1: Strategy validation gates
**Model:** sonnet

- Add `StrategyValidationKind` variants to `src/engine/schema.rs`.
- Implement `validate_primitive_params(strategies, registry) -> Vec<ValidationResult>`.
- Implement `validate_guard_required(strategies, registry) -> Vec<ValidationResult>`.
- Wire both into `src/daemon.rs` alongside the existing
  `validate_cross_references` call, before `CompositionEngine::new`.
- Any `Severity::Error` result triggers `fatal!` and daemon shutdown;
  warnings are logged.
- Unit tests: a strategy with missing required param errors; a strategy with
  an unknown param errors; a strategy with a `GuardRequired` primitive and
  no guard errors; a clean strategy passes.
- **Schema coverage lint (new):** `validate_primitive_params` only catches
  drift between *strategy* params and `input_schema`. It does NOT catch
  drift between `input_schema` and what the primitive's `execute()` body
  actually reads - a primitive can read `params.get("undeclared-key")`
  and the gate sees nothing. Mitigation: add a coverage-style test that
  scans each primitive's source for `params.get("X")` and `params["X"]`
  literals and asserts every X appears in that primitive's `input_schema`.
  Static grep is acceptable; the primitive source files are small and
  the literal keys are directly visible. This closes the "ghost param"
  hole the Architect flagged at round 1.
- These gates start dormant - no strategy trips them yet because the naming
  drift is at the handler layer, not the strategy-to-primitive layer. Phase
  2 onward is what makes them bite, so Phase 1 has to land clean.

#### Phase 2: Kebab-case IPC wire format for `agent.start`
**Model:** sonnet

- Add `AgentStartParams` typed struct inside
  `src/daemon/handlers/agent.rs` with
  `#[serde(rename_all = "kebab-case")]`. Fields: `agent_type: AgentKind`,
  `work_id: Option<String>`, `bundle_id: Option<String>`,
  `target_id: Option<String>`, `model: Option<String>`,
  `context_from: Option<String>`, `query: Option<String>`.
- Rewrite `handle_agent_start` to deserialize once and use typed fields.
- Rename the `role` param in `spawn-agent` primitive's `input_schema`
  (`src/primitive/catalog/agent.rs:59`) to `agent-type`. Expand the schema
  to declare every param the handler accepts: `agent-type` (required),
  `work-id`, `bundle-id`, `target-id`, `model`, `context-from`, `query`
  (all optional). The current schema omits `work-id` and `bundle-id`,
  which is why `agent-lifecycle.yml` would trip `UnknownParam` without
  this expansion.
- Update the `debug!` format string at `agent.rs:25` to match the new key.
- Rename `role` -> `agent-type` in every strategy that invokes
  `spawn-agent`. Known sites: `resources/decompose/strategies/default.yml`
  (decompose-plan, decompose-spec, decompose-phase),
  `resources/engine/strategies/agent-lifecycle.yml`,
  `resources/engine/strategies/recovery.yml`,
  `resources/engine/strategies/supervision.yml`. Confirm via
  `grep -rn 'role:' resources/engine/strategies resources/decompose/strategies`.
- Unit tests: the handler deserializes a kebab-case params blob correctly;
  rejects unknown keys if `deny_unknown_fields` is applied.
- Integration test: `decompose-plan` strategy actually spawns a decomposer.

#### Phase 3: Kebab-case sweep of remaining IPC handlers
**Model:** opus

- For every handler under `src/daemon/handlers/`, replace raw
  `params.get("foo_bar")` lookups with a typed params struct using
  `#[serde(rename_all = "kebab-case")]`.
- Known handler list (one per file, most already focused): `agent.rs`,
  `bundle.rs`, `chat.rs`, `director.rs`, `doc.rs`, `integrator.rs`,
  `learning.rs`, `lock.rs`, `phase.rs`, `plan.rs`, `spec.rs`, `tick.rs`,
  `work.rs`, plus the catch-all `mod.rs` and any remaining files.
- **Dynamic-payload handlers keep permissive shapes.** Primitives like
  `create-record` and `update-record` forward an arbitrary `fields`
  object directly into the IPC request (see
  `src/primitive/catalog/mutation.rs:33`). The handlers that receive
  these (`record.create`, `record.update`) must continue to deserialize
  into `serde_json::Value` or `HashMap<String, Value>` on the dynamic
  portion - `deny_unknown_fields` is wrong for them. Apply
  `deny_unknown_fields` ONLY to handlers with a closed schema (agent,
  bundle, lock, etc.). The per-handler commit decides.
- In-tree callers flip kebab-case in the same commit as the handler:
  `src/cli/dispatch.rs` today sends snake_case keys
  (`target_id`, `parent_id`, `files`, `acceptance_criteria` at lines
  114 and 290-302); `src/tui/run/ipc.rs` similarly. These are
  in-repo, not a version-skew surface - loopr is one binary, and the
  CLI/TUI/daemon ship together.
- Required key renames where the primitive/strategy and handler disagree
  on the *name* (not just case): produce the complete diff table during
  this phase. Known from current investigation: none other than
  `role` -> `agent-type`. Confirm by running the new `validate_primitive_params`
  gate after each handler conversion; the gate will flag any remaining
  drift as an unknown-param error.
- Decompose work: one handler per commit, compile + test after each.
- `otto ci` after each commit. Phase 1's validation gates will fire on any
  strategy that is no longer compatible; fix in the same commit.

#### Phase 4: CreateIntegrationBranch self-idempotency
**Model:** sonnet

- Pre-check `git show-ref --verify --quiet refs/heads/integration/<plan-id>`
  before running `git branch`. Exit 0 means the branch exists: return
  `Ok` with the existing branch name and a "branch already exists"
  summary. Exit 1 means absent: create it. Do NOT verify the branch
  base commit - doing so would wedge the plan the moment an agent
  commits to the integration branch or `main` advances from another
  plan's merge. The only question this primitive answers is "does the
  branch exist"; any other git-level divergence is a downstream
  problem and out of scope here.
- Downgrade `CreateIntegrationBranch::idempotency()` from `GuardRequired`
  to `Idempotent`. The primitive is intrinsically safe now; forcing every
  invoking strategy to carry a guard clause is ceremony without value.
  `GuardRequired` stays in the enum for primitives where only a strategy-
  side guard is coherent (e.g., `no-active-sessions` on `spawn-agent`).
- Consequence: `create-integration-branch-on-plan-active` in
  `git-lifecycle.yml` passes the new `validate_guard_required` gate with
  no guard clause - which is correct, because the primitive itself is
  now idempotent.
- Unit tests:
  1. Invoke twice back-to-back; both succeed; branch state unchanged.
  2. Create the branch manually, commit to it, then invoke the primitive;
     it returns Ok without touching the branch (proves no base-commit
     wedge).
  3. Advance `main` with an unrelated commit, then invoke the primitive;
     still returns Ok (proves no merge-base wedge).

#### Phase 5: Fix strategies that trip the new gates
**Model:** sonnet

- Run the daemon against the real strategy catalog and sweep the errors.
- Expected fallout after Phases 2-4: likely none, because Phase 2 fixes
  the one known naming drift and Phase 4 fixes the guard-required hole.
  Any additional drift surfaced by Phase 3's per-handler pass gets repaired
  as part of that phase's per-handler commit.
- The purpose of this phase is a clean-catalog sweep confirmation, not
  expected code change.

#### Phase 6: E2E re-validation
**Model:** sonnet

- Run `/e2e rust-version` end to end.
- Confirm the plan decomposes, spec / phase / work records materialize,
  and the director-threshold changes from the prior commit actually see
  work events.
- Report deltas against the prior failed e2e run.

## Alternatives Considered

### Alternative 1: Per-primitive `params_to_ipc` translation method

- **Description**: Each primitive that calls `ctx.bridge.request` overrides
  a new `params_to_ipc` method to rewrite kebab-case strategy params into
  snake_case IPC params. IPC handlers unchanged.
- **Pros**: No change to IPC wire format; strategies and handlers keep
  their respective existing conventions.
- **Cons**: Boilerplate per primitive (roughly 30 overrides). Introduces a
  per-primitive surface where naming drift can silently return - the next
  IPC handler rename requires remembering to update the translation in the
  primitive. The mapping logic also carries the `target-id` ambiguity
  (Implementer/Reviewer/thinking) into imperative code that is hard to
  validate.
- **Why not chosen**: Directive from review: IPC handlers should be
  brought into line with the project-wide kebab-case convention. One
  convention is cheaper long-term than a translation layer between two.

### Alternative 2: Normalize everything to snake_case end-to-end

- **Description**: Change strategy YAML convention from kebab-case to
  snake_case, align primitive input schemas to snake_case. One convention,
  no translation.
- **Pros**: No per-boundary serde attribute needed.
- **Cons**: Contradicts `rules/general.md`, which mandates kebab-case for
  YAML/JSON/config keys project-wide. Requires rewriting every strategy
  file. Every future YAML also has to remember the non-default convention.
- **Why not chosen**: Wrong direction. The kebab-case convention is the
  project standard; the IPC handler drifted from it, not the other way.

### Alternative 3: Universal kebab-to-snake conversion at the bridge layer

- **Description**: At the bridge-request boundary, automatically rewrite
  every `"foo-bar"` key to `"foo_bar"`. Every primitive keeps forwarding
  raw params, the conversion is invisible to both sides.
- **Pros**: Zero boilerplate at primitives or handlers.
- **Cons**: Does not handle semantic renames (`role` -> `agent_type`).
  Creates a seductive "it just works" illusion that hides real schema drift
  the next time an IPC handler is renamed. A test against the wrong key
  name passes silently.
- **Why not chosen**: Solves half the bug (case) and leaves the other half
  (name drift) invisible and undetected.

### Alternative 4: Runtime lazy validation at first invocation

- **Description**: Leave startup validation alone. When a strategy fires
  for the first time, validate then. Cache the result.
- **Pros**: No startup-time cost.
- **Cons**: Reproduces today's failure mode: the bug surfaces in production
  during the first real run. "Fail fast at startup" is the point of
  validation.
- **Why not chosen**: Indistinguishable from doing nothing.

## Technical Considerations

### Dependencies

- No new crates.
- Reuses existing `ValidationResult` / `Severity` plumbing in
  `src/engine/schema.rs`.
- Reuses existing `GuardConditionRegistry`.

### Performance

- Startup validation is O(strategies * steps * schema_fields). Small
  constant on a catalog of ~30 strategies. Nothing persistent.
- Serde-based kebab-case deserialization at handler boundaries adds a
  one-time per-request cost identical to the existing typed-params
  pattern elsewhere in the codebase. Negligible against the IPC round
  trip.
- No impact on the hot engine tick loop at runtime.

### Security

- No security-sensitive surface changes. Validation runs inside the daemon
  process on resources it already owns.

### Testing Strategy

- Unit tests per phase (see Implementation Plan).
- One focused integration test: load the real strategy catalog, call the two
  new validators, assert zero errors. This test becomes a permanent
  regression gate - any future strategy that trips the gates must be fixed
  before merging.
- E2E re-run of `/e2e rust-version` at the end of Phase 6 to prove the
  originally-failing loop now progresses through decomposition.

### Rollout Plan

- Single branch, sequential phases, one commit per phase.
- Phase 1 lands first and is dormant (no strategies will trip it until we
  touch them). Phase 5 sweeps whatever the gates flag.
- No feature flag; the gates are always on.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Phase 3 handler sweep converts a handler incorrectly and breaks CI | Med | Low | One handler per commit; `otto ci` after each. The new startup validation gate catches strategy mismatches as compile-time-like errors once Phase 1 lands. |
| A caller still sending legacy snake_case fails confusingly (`missing field 'agent-type'`) instead of being told it used the wrong case | Med | Low | Acceptable: this is the expected outcome. Every in-tree caller is converted in the same phase; external callers do not exist yet. |
| `#[serde(deny_unknown_fields)]` breaks a future feature where the daemon adds a new optional param and older callers omit it | Low | Low | Omitting a new optional field is fine with `deny_unknown_fields`; only sending an *unknown* field breaks. That breakage is loud and correct. |
| Self-idempotency in `create-integration-branch` masks a real git problem (branch exists but points elsewhere) | Low | Med | The check compares `git show-ref` against the expected base commit; mismatch returns an error, not a silent success. |
| Hidden strategy/primitive mismatches in primitives not exercised by `/e2e rust-version` stay latent | High | Med | Phase 1's `validate_primitive_params` catches these as startup errors the moment the daemon boots against the full strategy catalog. |
| In-tree callers (TUI, CLI, integration tests) send snake_case JSON to IPC handlers and break | High | Low | Inevitable during Phase 3, mitigated by bundling each handler's conversion with its caller updates in the same commit. `otto ci` after each commit catches stragglers. No version skew possible: all callers ship in the same binary. |
| Idempotency wedge in `create-integration-branch` if it verifies branch base state | Med | High | Resolved in design (architect round 1): the primitive's only check is `git show-ref --verify --quiet`. No base-commit comparison, no merge-base comparison. Branch exists -> Ok; absent -> create. |
| A primitive reads an undeclared param key, escaping `validate_primitive_params` | Med | Med | Phase 1 schema-coverage lint: static scan of each primitive's execute body for `params.get("X")` / `params["X"]` literals, asserting X is in `input_schema`. |

## Open Questions

- [ ] Should `validate_primitive_params` check param *types* against
  `InputField::field_type`, or only names? Today `input_schema` declares
  types but nothing consumes them. Starting with name-only is cheaper.
- [ ] After Phase 4 downgrades `CreateIntegrationBranch` to `Idempotent`,
  the 14 remaining `GuardRequired` primitives need a per-primitive audit:
  is each one genuinely un-self-idempotent, or was the declaration chosen
  by habit? Scoped to a follow-up design doc; not load-bearing for this
  one.
- [ ] Does the validator need to understand `$trigger.scope-id` and other
  interpolation tokens? Current design says no - validation happens on the
  literal param values, and interpolation is a runtime concern. Confirm no
  strategy declares a required param only as a literal overridden at
  runtime.
- [ ] Whether to apply `#[serde(deny_unknown_fields)]` universally. Pro:
  catches drift. Con: breaks forward-compatibility when the daemon adds a
  new optional field that older callers send with extra noise. Current
  lean: yes, enable it. The daemon and its callers are in the same repo;
  forward compatibility is not a real concern here.
- [ ] Whether the schema coverage lint in Phase 1 should be a build-time
  `cargo test` check, or a `build.rs` gate, or a custom `otto ci` step.
  Simplest: a `#[test]` that walks the primitive source files. Could
  migrate to a syn-based check later if the grep approach misses
  edge cases (e.g. `params["literal-key"]` indexing, dynamic key
  construction).

## References

- `docs/design/2026-04-16-director-agent.md` - preceding Director work that
  surfaced these bugs via e2e.
- `src/primitive/types.rs` - Primitive trait definition.
- `src/engine/schema.rs` - strategy schema + existing validators.
- `src/daemon.rs:290-330` - current startup validation gate.
- `resources/decompose/strategies/default.yml` - home of the `decompose-plan`
  strategy that triggered the discovery.
- `resources/engine/strategies/git-lifecycle.yml` - home of the integration
  branch strategy that demonstrated the guard-required gap.

## Architect Review Summary

This design passed two rounds of review with Gemini's Architect persona
before implementation began. Both rounds verified claims against the
actual codebase, not just the design text.

### Round 1: Four concerns raised

1. **Idempotency wedge** in `create-integration-branch`. The original
   Phase 4 proposed verifying the branch base commit against `main`,
   which would wedge the plan the moment either side advanced (agents
   committing to the integration branch, or another plan merging to
   main).
2. **Ghost optional params.** `validate_primitive_params` catches
   strategy->input_schema drift but not input_schema->execute() drift.
   A primitive could read a key it never declared; the gate would miss
   it.
3. **Dynamic data maps.** `#[serde(deny_unknown_fields)]` is incompatible
   with pass-through primitives like `create-record` whose payloads are
   intentionally dynamic.
4. **CLI/TUI caller boundary.** Snake_case callers in
   `src/cli/dispatch.rs` would break once handlers flipped to kebab-case.

### Round 1 resolutions (all folded into this doc)

1. Phase 4 simplified: existence check only. No base-commit or merge-base
   comparison. Three tests guard against wedge regressions.
2. Phase 1 gains a schema-coverage lint: static grep of each primitive's
   source for `params.get("X")` / `params["X"]` literals, asserting
   every X appears in `input_schema`. Added to tasks, Edge Cases, and
   Risks.
3. Phase 3 explicitly spares dynamic-payload handlers from
   `deny_unknown_fields`; only closed-schema handlers get the strict
   attribute.
4. Phase 3 bundles caller updates with handler updates per commit.
   No version-skew surface: loopr is one binary, CLI/TUI/daemon share
   the target, users always run what they built.

### Round 2 outcome

Architect independently verified each resolution against the codebase:

- `src/primitive/catalog/integration.rs` (mirrors `DeleteIntegrationBranch`
  pattern).
- `src/primitive/catalog/event.rs:55` (confirmed Architect's specific
  example was wrong; structural concern still valid).
- `src/primitive/catalog/mutation.rs:28` (confirmed dynamic forwarding).
- `src/main.rs` and `src/cli/dispatch.rs` (confirmed single-binary
  shipping).

Architect also independently swept `src/primitive/catalog/` for
`params.get(` / `params[` patterns and confirmed no dynamic key
construction exists anywhere, validating that a static-grep lint is
adequate and that a syn-based parser is over-engineered for this check.

Verbatim: *"The design is sound, structurally tight, and safe to
implement. Approval granted."*

---

## Architect Round 3 Findings (2026-04-17)

After Phase 6 shipped (v0.1.140), the Architect reviewed the implemented
sweep and surfaced four issues. Summary of disposition in this follow-up.

### Finding 1 (CRITICAL) — event payload casing mismatch — **FIXED**

The trigger engine could not match real agent events at runtime. Verified
the split:

- `src/agents/event.rs` had `#[serde(rename_all = "snake_case")]`, emitting
  `session_id` in payloads.
- `src/ipc/protocol.rs` emitted event name `"agent.status_changed"`
  (underscore).
- `src/trigger/schema.rs` registered `"agent.status-changed"` (hyphen);
  event name did not match.
- `src/trigger/evaluate.rs:254` reads scope id via
  `format!("{scope}-id")` (kebab); would never find `session_id`.
- Director matched snake; TUI matched snake but read kebab payload (broken).

Fix path (a): flipped to kebab everywhere. `AgentEvent` got
`rename_all_fields = "kebab-case"` (serde 1.0.172+ renames fields in
struct-like enum variants separately from variants). All eight
`agent.*_*` event-name literals flipped. Director match arm and payload
reads updated. Trigger schema `reconciliation-failed` corrected to
`reconciliation.failed`. Tests and one integration test file updated.
Commit: `fix(events): unify agent event naming on kebab-case`.

### Finding 2 — idempotency falsification — **FIXED (NonIdempotent)**

Three primitives had been downgraded to `Idempotent` with comments
admitting re-invocation doubles side effects:

- `retry-work` (`src/primitive/catalog/work.rs:298-305`) — always
  increments `attempt_count` and transitions to Ready.
- `combine-conflicting-works` (`src/primitive/catalog/conflict.rs:103`) —
  creates a new superseding Work on every call.
- `integrate-tick` (`src/primitive/catalog/integration.rs:322`) —
  allocates a Tick and performs git merges on every call.

Restored all three to `NonIdempotent` (the architect's allowed
alternative to `GuardRequired`). `NonIdempotent`'s stated requirement is
"must be last in action sequence or protected by cooldown"; all three
invocations satisfy "must be last":

- `work-retry-on-failure.on-success[0]`: retry-work is the last step.
- `handle-rejected-bundle.action[1]`: retry-work is the last step.
- `resolve-structural-conflict.action[0]`: combine-conflicting-works is
  the only step.
- `integrate-accepted-bundles.action[0]`: integrate-tick is the only step.

Why not `GuardRequired`: `handle-rejected-bundle` has `scope: bundle`
while retry-work targets a work-id from the trigger event payload. The
existing guard signature `(ctx, collection, id)` passes the strategy
scope + scope_id, so a work-level guard like
`no-active-implementer-for-work` cannot resolve the right record in
bundle-scoped strategies. A new `bundle-work-has-no-active-implementer`
guard that resolves bundle→work-id is the correct long-term fix; noted
as Round 4 work. `NonIdempotent` is accurate today and correctly drives
the architectural requirement that these primitives stay last in their
action sequence.

Regression test `engine::tests::strategy_catalog_passes_primitive_aware_gates`
continues to pass (the gate only inspects `GuardRequired`).

Commit: `fix(primitives): restore honest idempotency on
retry/combine/integrate`.

### Finding 3 — typed-struct handler sweep — **DEFERRED**

Phase 3 mandated `#[serde(rename_all = "kebab-case",
deny_unknown_fields)]` typed params structs per handler, but only 4 of
~19 handlers were converted (`AgentStartParams`, `BundleCreateParams`,
`LearningCreateParams`, `WorktreeRefreshParams`). The other ~15 still
use `params.get("foo-bar")` flips.

Deferred to a follow-up. Rationale:

- Architect explicitly marked this as correctness hardening, not a
  blocker.
- The E2E run (Finding 4) surfaced two pre-existing strategy-catalog
  bugs (see below) that are higher-value blockers. The typed-struct
  sweep should happen after those bugs are understood, since the
  remediation may touch the same handlers.
- 15 per-handler commits constitute a multi-session effort better
  scoped as its own design doc.

### Finding 4 — E2E rust-version end-to-end run — **COMPLETED, TWO NEW BUGS DISCOVERED**

Ran `/e2e rust-version` (600s timeout). Exit code 1 (Timeout). Plan
decomposed (pl-gmhj2) but into a runaway loop: 32 decomposer sessions
spawned, 31 duplicate Works created with identical titles, 0 Specs, 0
Phases, 0 Bundles, 0 implementers, 0 commits. Target `src/main.rs`
unchanged.

The Finding 1 event-casing fix did not regress behaviour; the engine
convergence loop hit its iteration limit 167 times across the run,
showing the trigger engine *is* matching events now. The loop itself
is caused by two pre-existing bugs in the strategy catalog, unrelated
to the primitive-wiring sweep:

**Bug A — brief-mode `plan-decomposable` infinite loop.**
`resources/engine/triggers/composites.yml:25` defines
`plan-decomposable` as `plan-is-active AND plan-active-no-specs`. Brief
mode plans (per `resources/decompose/roles/brief.yml`) produce Works
directly from the Plan and never gain Specs, so
`plan-active-no-specs` stays true forever. The strategy's
`no-active-sessions` guard only gates while a prior decomposer is
running; once each decomposer completes, a new one spawns. Fix likely
needs a composite like `plan-has-no-children-of-any-kind` or a
brief-mode-aware variant.

**Bug B — dead `restart-director-on-state` strategy.** Two warnings per
tick:

```
strategy 'restart-director-on-state' failed for '<plan-id>':
  check-threshold not supported for collection 'session'
strategy 'restart-director-on-state' on-failure wiring also failed:
  escalate failed: method not found: coordinator.escalate
```

The strategy references a `check-threshold` primitive call against the
`session` collection (unsupported) and an on-failure wiring that calls
an IPC method (`coordinator.escalate`) that no longer exists after the
coordinator → director rename. Strategy needs to be reshaped to use a
supported collection and wire to the current director-escalation IPC
surface, or removed if it no longer has a live role.

Both bugs are pre-existing (visible since before the primitive-wiring
sweep landed) and should be filed as separate tickets.
