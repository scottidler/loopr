# decomposer

Goal into validated Plan into Spec/Phase/Work DAG. The middle-end of the pipeline.

## In scope

- `fn plan(goal: &Goal, ctx: &mut Context) -> Result<Plan>`: user intent to validated Plan
- `fn decompose(plan: &Plan, ctx: &mut Context) -> Result<WorkDag>`: Plan to typed Work DAG
- `DecomposeStrategy` trait and impls (e.g. `BriefDecompose`, `FullDecompose`), selected by config name
- Dependency resolution and cycle detection on the produced DAG
- Decomposition-specific prompts under `src/prompts/`
- This crate's own `Config` struct

## Out of scope

- Agent execution (`agents`): decomposer produces the plan-of-work and stops
- Bundle review, integration (`agents`, `integrator`)
- LLM transport (`llm`), tool trait + impls (`tools`), worktree lifecycle (`worktree`)
- Record persistence (`store`), record definitions (`domain`)
- Whether to run a decomposer at all (that's the driver's call in `loopr`)

## Rule

Output of this crate is a `WorkDag` that downstream stages can consume without re-validating its structure. If execution-time code is checking "is this DAG well-formed?", the check belongs here, at produce-time.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
