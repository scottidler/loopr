# agents

Ralph Wiggum loops per role. The execute stages of the pipeline. Prompt assembly happens in `context` (a shared crate with `decomposer`); agents consume the assembled Messages and orchestrate the LLM/tool/worktree calls.

## In scope

- `run_implementer(work: &Work, deps: &Deps<...>) -> Result<Bundle>`
- `run_reviewer(bundle: &Bundle, deps: &Deps<...>) -> Result<Verdict>`
- `run_researcher(query: &Query, deps: &Deps<...>) -> Result<Finding>`
- `run_director(event: &Event, deps: &Deps<...>) -> Result<Action>`
- `RetryStrategy`, `EscalationStrategy` traits with named impls selected by config.
- Per-role `Config` sub-structs composed into this crate's `Config`.
- `Deps<L, T, W, S, C>` struct bundling the injected dependencies: `LlmClient`, `ToolExecutor`, `WorktreeManager`, `Store`, `ContextBuilder` (from `context`). See "Dependency injection" below.

## Out of scope

- **Prompt assembly — that's `context`.** Agents call `context::build_for_implementer(work, tools_snapshot)` to get a ready-to-send `Vec<Message>`; agents do not touch handlebars, `.pmt` files, or the three-layer override chain directly.
- Decomposition (`decomposer`), integration (`integrator`).
- LLM transport (`llm`), tool trait + impls (`tools`), worktree lifecycle (`worktree`), record persistence (`store`).
- Record types and FSM transitions (`domain`); agents transition records but the rules are enforced in `domain`.
- Which role to spawn when (that's the driver in `loopr`).

## Rule

A Ralph loop here takes a typed input, uses injected trait impls for side-effects, and returns a typed output. If a function is making orchestration decisions ("also spawn a reviewer", "escalate to director"), pull that into the driver in `loopr` and emit an event instead.

## Dependency injection: `Deps<L, T, W, S, C>` over bare generic params

Per `rules/rust.md`, agents are generic over their dependencies; `dyn` dispatch is forbidden. To avoid "contagious boilerplate" (Architect Round 3 warning) where every function drags `<L: LlmClient, T: ToolExecutor, W: WorktreeManager, S: Store, C: ContextBuilder>` through its signature, we use the `Deps` struct pattern that `rules/rust.md` already sanctions:

```rust
pub struct Deps<L, T, W, S, C>
where
    L: LlmClient,
    T: ToolExecutor,
    W: WorktreeManager,
    S: Store,
    C: ContextBuilder,
{
    pub llm: L,
    pub tools: T,
    pub worktrees: W,
    pub store: S,
    pub context: C,
}

pub fn run_implementer<L, T, W, S, C>(work: &Work, deps: &Deps<L, T, W, S, C>) -> Result<Bundle>
where
    L: LlmClient,
    T: ToolExecutor,
    W: WorktreeManager,
    S: Store,
    C: ContextBuilder,
{ ... }
```

One generic parameter flows through signatures; concrete trait bounds live on the struct definition. Tests construct `Deps` with fakes; production constructs with real impls. The junk-drawer concern that Architect raised is real but mitigated — not by splitting the crate prematurely, by keeping the dependency surface traceable through one type.

**Per-role crate split is a deferred option.** If `src/` pushes 1500 lines (see `rules/dealing-with-large-files.md`), first split per-role as module directories (`implementer/`, `reviewer/`, etc.); escalate to per-role sub-crates only if the module split proves insufficient.

## Instrumentation

Required scope fields, inherited by every `warn!` / `error!` emitted under an agents span:

- `work_id` — set by `run_implementer` (the outer span). Every `dispatch_action`, `propose_bundle`, lifeguard span, and inner git helper carries it via inheritance or as an explicit field.
- `iteration` — emitted as an event field on each iteration's `info!` ("implementer iteration start"). Not a span scope key today; the per-iteration body is async with early-returns and a span guard across awaits is unsafe in tokio. Events still carry it so log readers can group by iteration.
- `bundle_id` — set by `run_reviewer`'s outer span and by `propose_bundle`'s span.
- `action_kind` — set by `dispatch_action` and `check_action`; mirrors the serde tag (`run_tool`, `commit_changes`, `propose_bundle`, `done`, `need_help`).
- `action_hash`, `action_count`, `max_repeat` — set by `check_action`. Reading these from the span answers "which action repeated, and how many times?" without a debug-level rerun.

Levels: `run_implementer` and `run_reviewer` are `info`; per-iteration helpers (`dispatch_action`, `parse_actions`, `parse_verdict`, `check_action`, `commit_changes`, `propose_bundle`) are `debug`; subprocess wrappers (`run_git`, `rev_parse_head`, `compute_loc_changed`, `is_working_tree_clean`) are `trace` and carry `err`.

The acceptance test `tests/instrumentation.rs::agents_smoke_spans_lifeguard_escalation` drives the Stage 9 lifeguard-repeat shape and asserts the four entry-point spans exist with their required fields. If you remove an `#[instrument]` attribute or change a field name, that test fails.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
