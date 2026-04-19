# agents

Ralph Wiggum loops per role. The execute stages of the pipeline. Also the home of `ContextBuilder` — the only crate that sees `domain` + `store` + `llm` + `tools` + `worktree` simultaneously, which is exactly what prompt assembly requires.

## In scope

- `run_implementer(work: &Work, ctx: &Context) -> Result<Bundle>`
- `run_reviewer(bundle: &Bundle, ctx: &Context) -> Result<Verdict>`
- `run_researcher(query: &Query, ctx: &Context) -> Result<Finding>`
- `run_director(event: &Event, ctx: &mut Context) -> Result<Action>`
- **`ContextBuilder`** — token-budgeted prompt assembly. Renders `domain` records, persisted artifacts fetched via `store`, and `tools` schemas into the `Message` vector handed to `llm::LlmClient`. Uses the prompt templates shipped to `.loopr/prompts/` (handlebars-rust, partials for SSOT chunks).
- `RetryStrategy`, `EscalationStrategy` traits with named impls selected by config
- Role-specific prompts live in `.loopr/prompts/agents/` (per-target), populated from `include_dir!()` baked into the binary; fallback chain target → XDG → baked-in per vision.md
- Per-role `Config` sub-structs composed into this crate's `Config`

## Out of scope

- Decomposition (`decomposer`), integration (`integrator`)
- LLM transport (`llm`), tool trait + impls (`tools`), worktree lifecycle (`worktree`), record persistence (`store`)
- Record types and FSM transitions (`domain`); agents transition records but the rules are enforced in `domain`
- Which role to spawn when (that's the driver in `loopr`)

## Rule

A Ralph loop here takes a typed input, uses injected trait impls for side-effects, and returns a typed output. If a function is making orchestration decisions ("also spawn a reviewer", "escalate to director"), pull that into the driver in `loopr` and emit an event instead.

**Dependency injection via generics, not `dyn`.** Per `rules/rust.md`, agents are generic over their dependencies: `fn run_implementer<L: LlmClient, T: ToolExecutor, W: WorktreeManager, S: Store>(...)`. This lets tests inject fakes without dynamic dispatch.

**Agents is the widest-scope crate in v5.** It depends on `domain` + `store` + `llm` + `tools` + `worktree`. The Architect's Round 2 Q7 flagged this as a junk-drawer-in-waiting: four distinct side-effect domains orchestrated in one place, with a correspondingly large test-mock matrix. Mitigation lives here as a cultural rule:
- Trait-boundary fakes under `tests/fakes/` shared across ralph loops
- Individual ralph loop tests inject exactly the fakes they need
- If `src/` starts pushing 1500 lines (see `rules/dealing-with-large-files.md`), split per-role (`implementer/`, `reviewer/`, etc.) as module directories first; per-role crates only if the split proves insufficient

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
